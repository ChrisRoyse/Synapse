use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    ffi::OsString,
    path::{Path, PathBuf},
    process::{ExitCode, Stdio},
    sync::{
        Arc, Mutex, OnceLock, RwLock as StdRwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use axum::{
    Json,
    extract::{
        Query,
        rejection::JsonRejection,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use synapse_core::{Rect, SubsystemHealth, error_codes};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::Command,
    sync::{Notify, RwLock, oneshot},
    time::{sleep, timeout},
};
use uuid::Uuid;

const EXTENSION_ID: &str = "leoocgnkjnplbfdbklajepahofecgfbk";
const NATIVE_HOST_NAME: &str = "com.synapse.chrome_debugger";
const EXTENSION_ORIGIN: &str = "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk";
const BRIDGE_TOKEN_HEADER: &str = "x-synapse-bridge-token";
const DIRECT_HTTP_BRIDGE_CORS_ALLOW_METHODS: &str = "GET, POST, OPTIONS";
const DIRECT_HTTP_BRIDGE_CORS_ALLOW_HEADERS: &str =
    "content-type, x-synapse-bridge-token, x-synapse-bridge-register-token";
const BRIDGE_PROTOCOL_VERSION: u32 = 2;
const EXPECTED_EXTENSION_BUILD_ID: &str = "synapse-chrome-bridge-2026-08-12-noninterference-v19";
const EXPECTED_EXTENSION_DECLARED_BUILD_SHA256: &str =
    "14a9a7dd28a5ad63015265fa0b161884cff3976dd48e003a143a64967629db9a";
// >>> SHARED-CHROME-NATIVE-MESSAGE-BUDGET-CONTRACT
pub const NATIVE_MESSAGE_HTTP_BODY_LIMIT_MIB: usize = 64;
pub const PAGE_SCREENSHOT_NATIVE_MESSAGE_BUDGET_MIB: usize = 60;
// <<< SHARED-CHROME-NATIVE-MESSAGE-BUDGET-CONTRACT <<<
pub const NATIVE_MESSAGE_HTTP_BODY_LIMIT_BYTES: usize =
    NATIVE_MESSAGE_HTTP_BODY_LIMIT_MIB * 1024 * 1024;
pub const NATIVE_EVENT_HTTP_BODY_LIMIT_BYTES: usize = 1024 * 1024;
const COMMAND_TERMINAL_PAYLOAD_BUDGET_BYTES: usize =
    PAGE_SCREENSHOT_NATIVE_MESSAGE_BUDGET_MIB * 1024 * 1024;
const COMMAND_TERMINAL_RECEIPT_LIMIT: usize = 64;
const RECONNECT_WAKE_ALARM_NAME: &str = "synapse-daemon-bridge-reconnect";
const RECONNECT_WAKE_ALARM_PERIOD_MINUTES: f64 = 0.5;
const SYNAPSE_CHROME_BLOCKED_INSTALL_MESSAGE: &str = "Synapse blocked this extension on this host because debugger/nativeMessaging permissions can surface Chrome debugger or native-host popups during background automation.";
const REQUIRED_DIRECT_HTTP_CAPABILITIES: &[&str] = &[
    "alarmReconnect",
    "activateTab",
    "closeTab",
    "coordinateClick",
    "cookies",
    "downloads",
    "domAction",
    "frameLocators",
    "frames",
    "listTabs",
    "navigateTab",
    "openTab",
    "pageVitals",
    "pageContent",
    "setContent",
    "storageState",
    "ariaSnapshot",
    "assertPoll",
    "locateElements",
    "inspectElement",
    "scrollIntoView",
    "waitForText",
    "waitForLoadState",
    "waitForUrl",
    "waitForRequest",
    "waitForResponse",
    "waitForSelector",
    "clock",
    "pageEvents",
    "keyDispatch",
    "maintenancePauseReconnect",
    "reloadSelf",
    "targetInfo",
    "targetInfoPageText",
    "typeActiveElement",
    "setFieldValue",
];
/// Exact machine-readable failures the authenticated extension may send across
/// the command-response boundary. Keep this closed: arbitrary strings are not
/// trusted merely because they arrived in an `error.code` field. The canonical
/// lint gate compares this registry with the JavaScript runtime registry and
/// `synapse_core::error_codes` so cross-language drift cannot ship again
/// (#2217).
const TRUSTED_EXTENSION_ERROR_CODES: &[&str] = &[
    // >>> SHARED-CHROME-ERROR-CODE-CONTRACT
    "A11Y_CDP_ATTACH_FAILED",
    "A11Y_CDP_AXTREE_FAILED",
    "A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED",
    "A11Y_CDP_EXTENSION_DETACHED",
    "A11Y_CDP_EXTENSION_TIMEOUT",
    "A11Y_CDP_EXTENSION_UNAVAILABLE",
    "A11Y_CDP_RESPONSE_TOO_LARGE",
    "ACTION_TARGET_INVALID",
    "BROWSER_EVALUATE_JAVASCRIPT_EXCEPTION",
    "BROWSER_EVALUATE_TIMEOUT",
    "BROWSER_NAVIGATION_FAILED",
    "BROWSER_WAIT_TIMEOUT",
    "CAPTURE_PLAN_EXCEEDS_LIMIT",
    "CAPTURE_TARGET_INVALID",
    "CHROME_ACTIVE_ELEMENT_MISSING",
    "CHROME_ACTIVE_ELEMENT_NOT_EDITABLE",
    "CHROME_ACTIVE_ELEMENT_VALUE_MISMATCH",
    "CHROME_BEFOREINPUT_CANCELLED",
    "CHROME_BRIDGE_ERROR_CODE_CONTRACT_VIOLATION",
    "CHROME_BRIDGE_EXTENSION_STALE",
    "CHROME_BRIDGE_MESSAGE_BODY_EXCEEDS_LIMIT",
    "CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR",
    "CHROME_CAPTURE_VISIBLE_TAB_PENDING",
    "CHROME_CLOCK_FAILED",
    "CHROME_DOM_ACTION_POSTCONDITION_FAILED",
    "CHROME_DOM_ACTION_UNSUPPORTED",
    "CHROME_DOM_ELEMENT_AMBIGUOUS",
    "CHROME_DOM_ELEMENT_NOT_ACTIONABLE",
    "CHROME_DOM_ELEMENT_NOT_FOUND",
    "CHROME_DOM_SELECTOR_INVALID",
    "CHROME_FRAME_METADATA_FAILED",
    "CHROME_SCRIPTING_EMPTY_RESULT",
    "CHROME_SCRIPTING_EXECUTE_FAILED",
    "CHROME_SCRIPTING_UNAVAILABLE",
    "CHROME_SET_FIELD_BAD_LOCATOR",
    "CHROME_SET_FIELD_NOT_FOUND",
    "CHROME_SET_FIELD_NOT_UNIQUE",
    "CHROME_SET_FIELD_SELECTOR_INVALID",
    "CHROME_SET_FIELD_VALUE_MISMATCH",
    "CHROME_STORAGE_ACTION_FAILED",
    "CHROME_STORAGE_KEY_INVALID",
    "CHROME_STORAGE_OPERATION_UNSUPPORTED",
    "CHROME_STORAGE_STATE_LOAD_FAILED",
    "CHROME_STORAGE_STATE_READ_FAILED",
    "CHROME_WAIT_PREDICATE_INVALID",
    "PAGE_VITALS_READ_FAILED",
    "SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_PERSIST_FAILED",
    "SYNAPSE_CHROME_BRIDGE_RECONNECT_WAKE_ALARM_INVALID",
    "SYNAPSE_CHROME_DAEMON_UNAVAILABLE",
    "SYNAPSE_CHROME_EXTENSION_ID_MISMATCH",
    // <<< SHARED-CHROME-ERROR-CODE-CONTRACT <<<
];

fn trusted_extension_error_code(code: &str) -> Option<&'static str> {
    let index = TRUSTED_EXTENSION_ERROR_CODES.binary_search(&code).ok()?;
    Some(TRUSTED_EXTENSION_ERROR_CODES[index])
}
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const NATIVE_POLL_TIMEOUT: Duration = Duration::from_secs(15);
const DIRECT_WS_COMMAND_WAIT: Duration = Duration::from_secs(25);
const DEFAULT_RELOAD_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_RELOAD_WAIT_TIMEOUT_MS: u64 = 30_000;
const BACKGROUND_DEPLOY_PROCESS_TIMEOUT: Duration = Duration::from_secs(75);
const BACKGROUND_DEPLOY_JSON_PREFIX: &str = "SYNAPSE_CHROME_BRIDGE_INSTALLER_JSON_V1=";
const CHROME_PROFILE_SCAN_CACHE_TTL: Duration = Duration::from_secs(5);
const MAINTENANCE_RECONNECT_PAUSE_COMMAND: &str = "maintenancePauseReconnect";
const BACKGROUND_RUNTIME_RELOAD_COMMAND: &str = "reloadSelf";
const DEFAULT_MAINTENANCE_RECONNECT_PAUSE_MS: u64 = 120_000;
const MIN_MAINTENANCE_RECONNECT_PAUSE_MS: u64 = 1_000;
const MAX_MAINTENANCE_RECONNECT_PAUSE_MS: u64 = 900_000;
const DEFAULT_MAINTENANCE_RECONNECT_RESUME_PROBE_AFTER_MS: u64 = 30_000;
const RELOAD_RECONNECT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const NATIVE_DAEMON_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_NATIVE_MESSAGE_FROM_CHROME: usize = NATIVE_MESSAGE_HTTP_BODY_LIMIT_BYTES;
const MAX_NATIVE_MESSAGE_TO_CHROME: usize = 1024 * 1024;
const UNKNOWN_NATIVE_HOST_ID_FRAGMENT: &str = "unknown chrome debugger native host_id";
const INSTALL_GUIDANCE: &str = "install the bundled Synapse Chrome extension once through an explicit operator-authorized interaction, then use scripts\\install-synapse-chrome-debugger.ps1 -SkipAutoInstall for atomic background deployment and browser_debugger operation=reload_bridge for chrome.runtime.reload; unattended Synapse never focuses, activates, restores, minimizes, unminimizes, navigates, clicks, types into, or otherwise mutates a human Chrome window; the constant %LOCALAPPDATA%\\synapse\\chrome-extension\\active path survives browser/OS restart while the build ID and physical service-worker SHA prove the loaded bytes; expected_extension_id=leoocgnkjnplbfdbklajepahofecgfbk";
const NO_ACTIVE_HOST_REPAIR_GUIDANCE: &str = "no_active_host_repair=the background-only lifecycle requires an already-connected ready bridge host; restore it through a natural Chrome restart or an explicit operator-authorized install, then retry browser_debugger operation=reload_bridge; Synapse fails before touching any human tab or window";
const SETUP_REPAIR_MCP_GUIDANCE: &str = "mcp_setup_repair=call public MCP tool setup with operation=repair from profile=maintenance and repair.reason=chrome_bridge_build_skew";

#[derive(Clone, Debug)]
pub struct NativeHostInvocation {
    pub origin: String,
    pub parent_window: Option<String>,
}

#[must_use]
pub fn native_host_invocation_from_args<I>(args: I) -> Option<NativeHostInvocation>
where
    I: IntoIterator<Item = OsString>,
{
    let mut origin = None;
    let mut parent_window = None;
    for arg in args {
        let value = arg.to_string_lossy();
        if value.starts_with("chrome-extension://") {
            origin = Some(value.into_owned());
        } else if let Some(parent) = value.strip_prefix("--parent-window=") {
            parent_window = Some(parent.to_owned());
        }
    }
    origin.map(|origin| NativeHostInvocation {
        origin,
        parent_window,
    })
}

#[derive(Debug)]
pub struct ChromeDebuggerBridgeError {
    code: &'static str,
    detail: String,
}

impl ChromeDebuggerBridgeError {
    #[must_use]
    pub(crate) const fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    fn unavailable() -> Self {
        let profile_install_state = synapse_chrome_profile_install_state();
        Self {
            code: error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE,
            detail: format!(
                "Chrome debugger extension bridge is not connected; reason=no_active_chrome_bridge_host; repair_guidance={NO_ACTIVE_HOST_REPAIR_GUIDANCE}; {}; {INSTALL_GUIDANCE}",
                profile_install_state.detail
            ),
        }
    }

    fn timeout(command_kind: &str) -> Self {
        Self {
            code: error_codes::A11Y_CDP_EXTENSION_TIMEOUT,
            detail: format!(
                "Chrome debugger extension command {command_kind:?} timed out after {}s",
                COMMAND_TIMEOUT.as_secs()
            ),
        }
    }

    fn protocol(detail: impl Into<String>) -> Self {
        Self {
            code: error_codes::A11Y_CDP_ATTACH_FAILED,
            detail: detail.into(),
        }
    }

    fn host_reload_failed(detail: impl Into<String>) -> Self {
        Self {
            code: error_codes::CHROME_BRIDGE_HOST_RELOAD_FAILED,
            detail: detail.into(),
        }
    }

    fn params_invalid(detail: impl Into<String>) -> Self {
        Self {
            code: error_codes::TOOL_PARAMS_INVALID,
            detail: detail.into(),
        }
    }

    fn extension(code: Option<&str>, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        if let Some(code) = code.and_then(trusted_extension_error_code) {
            return Self { code, detail };
        }
        Self {
            code: error_codes::CHROME_BRIDGE_ERROR_CODE_CONTRACT_VIOLATION,
            detail: format!(
                "Chrome debugger extension error-code contract violated; received_code={code:?} original_detail={detail:?}; remediation=register the exact machine code in PUBLIC_COMMAND_ERROR_CODES, TRUSTED_EXTENSION_ERROR_CODES, and synapse_core::error_codes in one change, then bump/redeploy the bridge build identity"
            ),
        }
    }

    fn stale(command_kind: &str, host_id: &str, host: &HostRecord, reason: &str) -> Self {
        let profile_install_state = synapse_chrome_profile_install_state();
        let expected_worker_sha256 = profile_install_state
            .active_profile_service_worker_sha256
            .as_deref()
            .unwrap_or("not_available");
        let expected_worker_path = profile_install_state
            .active_profile_extension_path
            .as_ref()
            .map_or_else(
                || "<none>".to_owned(),
                |path| quote_detail_value(&path.join("service_worker.js").to_string_lossy()),
            );
        let remediation = if reason.contains("capture_visible_tab_lease_quarantined") {
            "fully restart Chrome so chrome.storage.session establishes a new browser-session identity proving the prior uncancellable Chrome API promise cannot still be live; do not reload only the extension or retry the capture"
        } else {
            "call browser_debugger operation=reload_bridge through the public facade after setting profile=browser_debugger; the daemon atomically deploys the bridge in a hidden process, asks the connected extension to call chrome.runtime.reload, and separately verifies the profile row, service-worker SHA, and replacement authenticated host without touching a human Chrome window"
        };
        Self {
            code: error_codes::CHROME_BRIDGE_EXTENSION_STALE,
            detail: format!(
                "Chrome bridge extension is stale for command {command_kind:?}; host_id={host_id} reason={reason} extension_id={} extension_version={} extension_protocol_version={} extension_build_id={} extension_declared_build_sha256={} extension_service_worker_sha256={} extension_service_worker_sha256_status={} extension_service_worker_sha256_error={} expected_build_id={} expected_service_worker_sha256={} expected_service_worker_path={} capabilities={} required_capabilities={} setup_repair_guidance={} remediation={remediation}",
                host.extension_id.as_deref().unwrap_or("not_seen_yet"),
                host.extension_version.as_deref().unwrap_or("not_seen_yet"),
                host.extension_protocol_version
                    .map_or_else(|| "not_seen_yet".to_owned(), |value| value.to_string()),
                host.extension_build_id.as_deref().unwrap_or("not_seen_yet"),
                host.extension_declared_build_sha256
                    .as_deref()
                    .unwrap_or("not_seen_yet"),
                host.extension_service_worker_sha256
                    .as_deref()
                    .unwrap_or("not_seen_yet"),
                host.extension_service_worker_sha256_status
                    .as_deref()
                    .unwrap_or("not_seen_yet"),
                host.extension_service_worker_sha256_error
                    .as_deref()
                    .unwrap_or("none"),
                EXPECTED_EXTENSION_BUILD_ID,
                expected_worker_sha256,
                expected_worker_path,
                format_capabilities(&host.extension_capabilities),
                REQUIRED_DIRECT_HTTP_CAPABILITIES.join(","),
                setup_repair_guidance()
            ),
        }
    }

    fn normal_bridge_attach_disabled(hwnd: i64, command_kind: &str) -> Self {
        let external_surface_hint = external_chrome_surface_hint();
        Self {
            code: error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
            detail: format!(
                "Synapse Chrome Bridge refused debugger-backed command {command_kind:?} before queueing any Chrome command; hwnd={hwnd} reason=the normal authenticated profile permanently forbids debugger and nativeMessaging permissions because an attachment can display a layout-shifting infobar{external_surface_hint} remediation=launch a session-owned raw-CDP target in Synapse's dedicated non-default automation profile and invoke the same public capability there"
            ),
        }
    }
}

fn external_chrome_surface_hint() -> String {
    let rows = external_chrome_popup_risks();
    if rows.is_empty() {
        return String::new();
    }
    format!(
        " external_chrome_popup_risk={}",
        format_external_chrome_popup_risks(&rows)
    )
}

fn external_chrome_popup_risks() -> Vec<String> {
    let mut rows = chrome_profile_scan().scan.external_profile_surfaces;
    rows.extend(external_chrome_native_messaging_processes());
    rows.sort();
    rows.dedup();
    rows
}

fn synapse_chrome_self_profile_surfaces() -> Vec<String> {
    chrome_profile_scan().scan.self_profile_surfaces
}

fn synapse_chrome_self_active_popup_risks(rows: &[String]) -> Vec<String> {
    rows.iter()
        .filter(|row| row.contains("risk_basis=active_or_manifest_hazard_without_disable_reason"))
        .cloned()
        .collect()
}

fn synapse_chrome_self_permission_warning(rows: &[String], policy_shield_present: bool) -> String {
    if rows.is_empty() {
        return "synapse_chrome_bridge_permission_warning=false self_risk_count=0".to_owned();
    }
    let active_rows = synapse_chrome_self_active_popup_risks(rows);
    let formatted = format_external_chrome_popup_risks(rows);
    if !active_rows.is_empty() {
        return format!(
            "synapse_chrome_bridge_permission_blocking=true self_risk_scope=active_synapse_bridge_debugger_or_native_messaging_permission self_risk_count={} self_active_risk_count={} synapse_chrome_bridge_permission_risk={} remediation=normal bridge commands fail closed while the Synapse extension profile row still exposes active/manifest debugger or nativeMessaging; deploy the popup-free bridge and reload it through the authenticated background chrome.runtime.reload path",
            rows.len(),
            active_rows.len(),
            formatted
        );
    }
    let shield_scope = if policy_shield_present {
        "granted_only_stale_permissions_with_policy_shield"
    } else {
        "granted_only_stale_permissions_without_policy_shield"
    };
    format!(
        "synapse_chrome_bridge_permission_warning=true self_risk_scope={shield_scope} self_risk_count={} synapse_chrome_bridge_permission_risk={} remediation=granted-only profile residue is not active Chrome extension capability once the loaded popup-free bridge reports extension_debugger_api_available=false; commands rely on exact bridge identity plus active/manifest debugger/nativeMessaging fail-closed gates",
        rows.len(),
        formatted
    )
}

#[derive(Clone, Debug)]
struct SynapseChromeProfileInstallState {
    detail: String,
    active_profile_extension_path: Option<PathBuf>,
    active_profile_service_worker_sha256: Option<String>,
    active_profile_service_worker_error: Option<String>,
}

#[derive(Clone, Debug)]
struct ChromeProfileScan {
    install_state: SynapseChromeProfileInstallState,
    self_profile_surfaces: Vec<String>,
    external_profile_surfaces: Vec<String>,
}

impl ChromeProfileScan {
    fn not_scanned(reason: &str) -> Self {
        Self {
            install_state: SynapseChromeProfileInstallState::not_scanned(reason),
            self_profile_surfaces: Vec::new(),
            external_profile_surfaces: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct ChromeProfileScanCacheEntry {
    captured_at: Instant,
    captured_unix_ms: u64,
    epoch: u64,
    scan: ChromeProfileScan,
}

#[derive(Clone, Debug)]
struct ChromeProfileScanRead {
    scan: ChromeProfileScan,
    cache_status: &'static str,
    cache_age_ms: u64,
    cache_epoch: u64,
    cache_captured_unix_ms: u64,
}

impl ChromeProfileScanRead {
    fn install_state(&self) -> SynapseChromeProfileInstallState {
        let mut state = self.scan.install_state.clone();
        state.detail = format!(
            "{} profile_scan_cache_status={} profile_scan_cache_age_ms={} profile_scan_cache_ttl_ms={} profile_scan_cache_epoch={} profile_scan_cache_captured_unix_ms={}",
            state.detail,
            self.cache_status,
            self.cache_age_ms,
            duration_millis_u64(CHROME_PROFILE_SCAN_CACHE_TTL),
            self.cache_epoch,
            self.cache_captured_unix_ms,
        );
        state
    }
}

fn chrome_profile_scan_cache() -> &'static StdRwLock<Option<ChromeProfileScanCacheEntry>> {
    static CACHE: OnceLock<StdRwLock<Option<ChromeProfileScanCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| StdRwLock::new(None))
}

fn chrome_profile_scan_cache_epoch() -> &'static AtomicU64 {
    static EPOCH: AtomicU64 = AtomicU64::new(0);
    &EPOCH
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn chrome_profile_scan() -> ChromeProfileScanRead {
    let now = Instant::now();
    let cache = chrome_profile_scan_cache();
    match cache.read() {
        Ok(guard) => {
            if let Some(entry) = guard.as_ref() {
                let age = now.saturating_duration_since(entry.captured_at);
                if age <= CHROME_PROFILE_SCAN_CACHE_TTL {
                    return ChromeProfileScanRead {
                        scan: entry.scan.clone(),
                        cache_status: "hit",
                        cache_age_ms: duration_millis_u64(age),
                        cache_epoch: entry.epoch,
                        cache_captured_unix_ms: entry.captured_unix_ms,
                    };
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                code = "CHROME_PROFILE_SCAN_CACHE_READ_POISONED",
                detail = %error,
                "Chrome profile scan cache read lock poisoned; bypassing cache"
            );
            return ChromeProfileScanRead {
                scan: scan_chrome_profiles_uncached(),
                cache_status: "lock_poisoned",
                cache_age_ms: 0,
                cache_epoch: chrome_profile_scan_cache_epoch().load(Ordering::Relaxed),
                cache_captured_unix_ms: now_unix_ms(),
            };
        }
    }

    match cache.write() {
        Ok(mut guard) => {
            let now = Instant::now();
            if let Some(entry) = guard.as_ref() {
                let age = now.saturating_duration_since(entry.captured_at);
                if age <= CHROME_PROFILE_SCAN_CACHE_TTL {
                    return ChromeProfileScanRead {
                        scan: entry.scan.clone(),
                        cache_status: "hit_after_wait",
                        cache_age_ms: duration_millis_u64(age),
                        cache_epoch: entry.epoch,
                        cache_captured_unix_ms: entry.captured_unix_ms,
                    };
                }
            }
            let scan = scan_chrome_profiles_uncached();
            let captured_unix_ms = now_unix_ms();
            let epoch = chrome_profile_scan_cache_epoch().load(Ordering::Relaxed);
            *guard = Some(ChromeProfileScanCacheEntry {
                captured_at: now,
                captured_unix_ms,
                epoch,
                scan: scan.clone(),
            });
            ChromeProfileScanRead {
                scan,
                cache_status: "miss",
                cache_age_ms: 0,
                cache_epoch: epoch,
                cache_captured_unix_ms: captured_unix_ms,
            }
        }
        Err(error) => {
            tracing::warn!(
                code = "CHROME_PROFILE_SCAN_CACHE_WRITE_POISONED",
                detail = %error,
                "Chrome profile scan cache write lock poisoned; bypassing cache"
            );
            ChromeProfileScanRead {
                scan: scan_chrome_profiles_uncached(),
                cache_status: "lock_poisoned",
                cache_age_ms: 0,
                cache_epoch: chrome_profile_scan_cache_epoch().load(Ordering::Relaxed),
                cache_captured_unix_ms: now_unix_ms(),
            }
        }
    }
}

fn invalidate_chrome_profile_scan_cache(reason: &str) {
    let epoch = chrome_profile_scan_cache_epoch().fetch_add(1, Ordering::Relaxed) + 1;
    match chrome_profile_scan_cache().write() {
        Ok(mut guard) => {
            let had_entry = guard.is_some();
            *guard = None;
            tracing::info!(
                code = "CHROME_PROFILE_SCAN_CACHE_INVALIDATED",
                reason,
                had_entry,
                epoch,
                "Chrome profile scan cache invalidated"
            );
        }
        Err(error) => {
            tracing::warn!(
                code = "CHROME_PROFILE_SCAN_CACHE_INVALIDATE_POISONED",
                reason,
                detail = %error,
                epoch,
                "Chrome profile scan cache invalidation failed because lock is poisoned"
            );
        }
    }
}

impl SynapseChromeProfileInstallState {
    fn not_scanned(reason: &str) -> Self {
        Self {
            detail: format!(
                "synapse_chrome_bridge_profile_installation scanned=false installed=unknown reason={reason}"
            ),
            active_profile_extension_path: None,
            active_profile_service_worker_sha256: None,
            active_profile_service_worker_error: Some(reason.to_owned()),
        }
    }
}

fn synapse_chrome_profile_install_state() -> SynapseChromeProfileInstallState {
    chrome_profile_scan().install_state()
}

fn scan_chrome_profiles_uncached() -> ChromeProfileScan {
    let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") else {
        return ChromeProfileScan::not_scanned("localappdata_missing");
    };
    let user_data_root = PathBuf::from(local_appdata)
        .join("Google")
        .join("Chrome")
        .join("User Data");
    let active_profile_candidates = chrome_active_profile_candidates(&user_data_root);
    let Ok(profile_dirs) = std::fs::read_dir(&user_data_root) else {
        return ChromeProfileScan {
            install_state: SynapseChromeProfileInstallState {
                detail: format!(
                    "synapse_chrome_bridge_profile_installation scanned=false installed=unknown user_data_root={} reason=user_data_root_unreadable",
                    quote_detail_value(&user_data_root.to_string_lossy())
                ),
                active_profile_extension_path: None,
                active_profile_service_worker_sha256: None,
                active_profile_service_worker_error: Some("user_data_root_unreadable".to_owned()),
            },
            self_profile_surfaces: Vec::new(),
            external_profile_surfaces: Vec::new(),
        };
    };

    let mut profile_count = 0_usize;
    let mut parse_error_count = 0_usize;
    let mut profile_dir_error_count = 0_usize;
    let mut profile_file_type_error_count = 0_usize;
    let mut preference_read_error_count = 0_usize;
    let mut installed_profiles = BTreeSet::new();
    let mut installed_profile_paths: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut self_profile_surfaces = Vec::new();
    let mut external_profile_surfaces = Vec::new();
    for profile_dir in profile_dirs {
        let Ok(profile_dir) = profile_dir else {
            profile_dir_error_count += 1;
            continue;
        };
        let Ok(file_type) = profile_dir.file_type() else {
            profile_file_type_error_count += 1;
            continue;
        };
        if !file_type.is_dir() || profile_dir.file_name() == "Snapshots" {
            continue;
        }
        let profile = profile_dir.file_name().to_string_lossy().into_owned();
        let profile_pref_paths = ["Preferences", "Secure Preferences"]
            .into_iter()
            .map(|pref_file| (pref_file, profile_dir.path().join(pref_file)))
            .collect::<Vec<_>>();
        if !profile_pref_paths.iter().any(|(_, path)| path.is_file()) {
            continue;
        }
        profile_count += 1;
        let mut profile_installed = false;
        let mut runtime_by_id: HashMap<String, ChromeExtensionRuntimeState> = HashMap::new();
        for (pref_file, pref_path) in profile_pref_paths {
            let Ok(raw) = std::fs::read_to_string(&pref_path) else {
                preference_read_error_count += 1;
                continue;
            };
            let Ok(pref) = serde_json::from_str::<Value>(&raw) else {
                parse_error_count += 1;
                self_profile_surfaces.push(format!(
                    "profile={profile} pref={pref_file} parse_error=true"
                ));
                external_profile_surfaces.push(format!(
                    "profile={profile} pref={pref_file} parse_error=true"
                ));
                continue;
            };
            if let Some(setting) = pref
                .get("extensions")
                .and_then(|value| value.get("settings"))
                .and_then(|settings| settings.get(EXTENSION_ID))
            {
                profile_installed = true;
                let mut runtime_state = chrome_extension_runtime_state(setting);
                if pref_file == "Preferences" {
                    runtime_by_id.insert(EXTENSION_ID.to_owned(), runtime_state.clone());
                } else if let Some(preferences_runtime_state) = runtime_by_id.get(EXTENSION_ID) {
                    runtime_state = preferences_runtime_state.clone();
                }
                let active_permissions = active_api_permissions(setting);
                let manifest_permissions = manifest_api_permissions(setting);
                let granted_permissions = granted_api_permissions(setting);
                let active_or_manifest_hazards = synapse_self_hazard_api_permissions(
                    active_permissions
                        .iter()
                        .chain(manifest_permissions.iter())
                        .map(String::as_str),
                );
                let granted_hazards = synapse_self_hazard_api_permissions(
                    granted_permissions.iter().map(String::as_str),
                );
                if !active_or_manifest_hazards.is_empty() || !granted_hazards.is_empty() {
                    let disabled =
                        !runtime_state.disable_reasons.is_empty() || runtime_state.state == Some(0);
                    let active_hazard_enabled = !disabled && !active_or_manifest_hazards.is_empty();
                    let granted_only =
                        active_or_manifest_hazards.is_empty() && !granted_hazards.is_empty();
                    if active_hazard_enabled || granted_only {
                        let risk_basis = if active_hazard_enabled {
                            "active_or_manifest_hazard_without_disable_reason"
                        } else {
                            "granted_only_stale"
                        };
                        self_profile_surfaces.push(format!(
                            "profile={profile} pref={pref_file} extension_id={EXTENSION_ID} name=\"Synapse Chrome Bridge\" active_api={} manifest_api={} granted_hazard_api={} synapse_self_popup_risk=true risk_basis={risk_basis} state={} active_bit={} disable_reasons={}",
                            active_permissions.join(","),
                            manifest_permissions.join(","),
                            granted_hazards.join(","),
                            runtime_state
                                .state.map_or_else(|| "<absent>".to_owned(), |value| value.to_string()),
                            runtime_state
                                .active_bit.map_or_else(|| "<absent>".to_owned(), |value| value.to_string()),
                            format_disable_reasons(&runtime_state.disable_reasons)
                        ));
                    }
                }
                if let Some(extension_path) = setting
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|path| !path.trim().is_empty())
                {
                    installed_profile_paths
                        .entry(profile.clone())
                        .or_default()
                        .insert(extension_path.to_owned());
                }
            }
            let Some(settings) = pref
                .get("extensions")
                .and_then(|value| value.get("settings"))
                .and_then(Value::as_object)
            else {
                continue;
            };
            for (extension_id, setting) in settings {
                if extension_id == EXTENSION_ID {
                    continue;
                }
                let mut runtime_state = chrome_extension_runtime_state(setting);
                if pref_file == "Preferences" {
                    runtime_by_id.insert(extension_id.clone(), runtime_state.clone());
                } else if let Some(preferences_runtime_state) = runtime_by_id.get(extension_id) {
                    runtime_state = preferences_runtime_state.clone();
                }
                let active_permissions = active_api_permissions(setting);
                let manifest_permissions = manifest_api_permissions(setting);
                let granted_permissions = granted_api_permissions(setting);
                let active_or_manifest_hazards = hazard_api_permissions(
                    active_permissions
                        .iter()
                        .chain(manifest_permissions.iter())
                        .map(String::as_str),
                );
                let granted_hazards =
                    hazard_api_permissions(granted_permissions.iter().map(String::as_str));
                if active_or_manifest_hazards.is_empty() && granted_hazards.is_empty() {
                    continue;
                }
                if !external_popup_risk_enabled(
                    &runtime_state,
                    !active_or_manifest_hazards.is_empty(),
                    !granted_hazards.is_empty(),
                ) {
                    continue;
                }
                let risk_basis = if active_or_manifest_hazards.is_empty() {
                    "state_enabled_granted_hazard"
                } else if runtime_state.state == Some(1) {
                    "state_enabled_active_or_manifest_hazard"
                } else {
                    "active_or_manifest_hazard_without_disable_reason"
                };
                let name = setting
                    .get("manifest")
                    .and_then(|manifest| manifest.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("<unnamed>");
                external_profile_surfaces.push(format!(
                    "profile={profile} pref={pref_file} extension_id={extension_id} name={name:?} active_api={} manifest_api={} granted_hazard_api={} popup_risk=true risk_basis={risk_basis} state={} active_bit={} disable_reasons={}",
                    active_permissions.join(","),
                    manifest_permissions.join(","),
                    granted_hazards.join(","),
                    runtime_state
                        .state.map_or_else(|| "<absent>".to_owned(), |value| value.to_string()),
                    runtime_state
                        .active_bit.map_or_else(|| "<absent>".to_owned(), |value| value.to_string()),
                    format_disable_reasons(&runtime_state.disable_reasons)
                ));
            }
        }
        if profile_installed {
            installed_profiles.insert(profile);
        }
    }
    self_profile_surfaces.sort();
    self_profile_surfaces.dedup();
    external_profile_surfaces.sort();
    external_profile_surfaces.dedup();

    let installed_profile_count = installed_profiles.len();
    let installed = installed_profile_count > 0;
    let active_profile = resolve_synapse_active_chrome_profile(
        &user_data_root,
        &active_profile_candidates,
        &installed_profiles,
    );
    let active_profile_installed = active_profile
        .as_ref()
        .map(|profile| installed_profiles.contains(profile));
    let active_profile_extension_path = active_profile
        .as_ref()
        .and_then(|profile| installed_profile_paths.get(profile))
        .and_then(|paths| {
            paths
                .iter()
                .map(PathBuf::from)
                .find(|path| path.join("service_worker.js").is_file())
                .or_else(|| paths.iter().next().map(PathBuf::from))
        });
    let (active_profile_service_worker_sha256, active_profile_service_worker_error) =
        match active_profile_extension_path.as_ref() {
            Some(extension_path) => {
                let worker_path = extension_path.join("service_worker.js");
                match sha256_file_hex_lower(&worker_path) {
                    Ok(hash) => (Some(hash), None),
                    Err(error) => (None, Some(format!("service_worker_hash_error={error}"))),
                }
            }
            None => (
                None,
                Some("active_profile_extension_path_missing".to_owned()),
            ),
        };
    let reason = if profile_count == 0 {
        "no_profile_dirs"
    } else if !installed {
        "extension_id_absent_from_preferences_and_secure_preferences"
    } else if active_profile_installed == Some(false) {
        "active_profile_missing_extension"
    } else {
        "extension_profile_row_present"
    };
    let active_profile_detail = active_profile
        .as_deref()
        .map_or_else(|| "<unknown>".to_owned(), quote_detail_value);
    let active_profile_installed_detail =
        active_profile_installed.map_or_else(|| "unknown".to_owned(), |value| value.to_string());
    let installed_profile_detail = if installed_profiles.is_empty() {
        "<none>".to_owned()
    } else {
        installed_profiles
            .iter()
            .map(|profile| quote_detail_value(profile))
            .collect::<Vec<_>>()
            .join(",")
    };
    let active_profile_extension_path_detail = active_profile_extension_path.as_ref().map_or_else(
        || "<none>".to_owned(),
        |path| quote_detail_value(&path.to_string_lossy()),
    );
    let active_profile_service_worker_sha256_detail = active_profile_service_worker_sha256
        .as_deref()
        .unwrap_or("not_available");
    let active_profile_service_worker_error_detail = active_profile_service_worker_error
        .as_deref()
        .unwrap_or("none");

    ChromeProfileScan {
        install_state: SynapseChromeProfileInstallState {
            detail: format!(
                "synapse_chrome_bridge_profile_installation scanned=true installed={} user_data_root={} profile_count={} installed_profile_count={} installed_profiles={} active_profile={} active_profile_installed={} active_profile_extension_path={} active_profile_service_worker_sha256={} active_profile_service_worker_error={} profile_dir_error_count={} profile_file_type_error_count={} preference_read_error_count={} parse_error_count={} reason={} browser_debugger_reload_bridge_can_install_absent_extension=true remediation=call browser_debugger operation=reload_bridge with profile=browser_debugger; the host-controlled path atomically deploys the bundled bridge into the constant %LOCALAPPDATA%\\synapse\\chrome-extension\\active directory, invokes the exact Load unpacked or Reload control in the already-open active profile, and separately verifies the profile row plus new authenticated host",
                installed,
                quote_detail_value(&user_data_root.to_string_lossy()),
                profile_count,
                installed_profile_count,
                installed_profile_detail,
                active_profile_detail,
                active_profile_installed_detail,
                active_profile_extension_path_detail,
                active_profile_service_worker_sha256_detail,
                active_profile_service_worker_error_detail,
                profile_dir_error_count,
                profile_file_type_error_count,
                preference_read_error_count,
                parse_error_count,
                reason
            ),
            active_profile_extension_path,
            active_profile_service_worker_sha256,
            active_profile_service_worker_error,
        },
        self_profile_surfaces,
        external_profile_surfaces,
    }
}

fn chrome_active_profile_candidates(user_data_root: &std::path::Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(user_data_root.join("Local State")) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    if let Some(last_used) = parsed
        .get("profile")
        .and_then(|profile| profile.get("last_used"))
        .and_then(Value::as_str)
    {
        candidates.push(last_used.to_owned());
    }
    if let Some(last_active_profiles) = parsed
        .get("profile")
        .and_then(|profile| profile.get("last_active_profiles"))
        .and_then(Value::as_array)
    {
        for candidate in last_active_profiles.iter().filter_map(Value::as_str) {
            candidates.push(candidate.to_owned());
        }
    }
    let mut seen = BTreeSet::new();
    candidates.retain(|candidate| seen.insert(candidate.clone()));
    candidates
}

fn resolve_synapse_active_chrome_profile(
    user_data_root: &std::path::Path,
    active_profile_candidates: &[String],
    installed_profiles: &BTreeSet<String>,
) -> Option<String> {
    for candidate in active_profile_candidates {
        if installed_profiles.contains(candidate) {
            return Some(candidate.clone());
        }
    }
    if installed_profiles.len() == 1 {
        return installed_profiles.iter().next().cloned();
    }
    active_profile_candidates
        .iter()
        .find(|candidate| user_data_root.join(candidate).is_dir())
        .cloned()
}

#[derive(Clone, Debug)]
struct SynapseChromeSelfPolicyShieldStatus {
    present: bool,
    detail: String,
}

#[cfg(windows)]
fn synapse_chrome_self_policy_shield_status() -> SynapseChromeSelfPolicyShieldStatus {
    let policy_write_access =
        chrome_policy_set_value_access_status(r"Software\Policies\Google\Chrome");
    let Some(raw) =
        read_hkcu_registry_string(r"Software\Policies\Google\Chrome", "ExtensionSettings")
    else {
        return SynapseChromeSelfPolicyShieldStatus {
            present: false,
            detail: format!(
                "synapse_chrome_self_policy_shield_present=false policy_hive=HKCU policy_path=Software\\Policies\\Google\\Chrome value=ExtensionSettings reason=value_missing_or_unreadable {policy_write_access}"
            ),
        };
    };
    if raw.trim().is_empty() {
        return SynapseChromeSelfPolicyShieldStatus {
            present: false,
            detail: format!(
                "synapse_chrome_self_policy_shield_present=false policy_hive=HKCU policy_path=Software\\Policies\\Google\\Chrome value=ExtensionSettings reason=value_empty {policy_write_access}"
            ),
        };
    }
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return SynapseChromeSelfPolicyShieldStatus {
            present: false,
            detail: format!(
                "synapse_chrome_self_policy_shield_present=false policy_hive=HKCU policy_path=Software\\Policies\\Google\\Chrome value=ExtensionSettings reason=parse_error raw_len={} {policy_write_access}",
                raw.len(),
            ),
        };
    };
    let Some(entry) = parsed.get(EXTENSION_ID) else {
        return SynapseChromeSelfPolicyShieldStatus {
            present: false,
            detail: format!(
                "synapse_chrome_self_policy_shield_present=false policy_hive=HKCU policy_path=Software\\Policies\\Google\\Chrome value=ExtensionSettings reason=self_entry_missing {policy_write_access}"
            ),
        };
    };
    let blocked = entry
        .get("blocked_permissions")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let has_native_messaging = blocked
        .iter()
        .any(|permission| permission == "nativeMessaging");
    let marker_matches = entry.get("blocked_install_message").and_then(Value::as_str)
        == Some(SYNAPSE_CHROME_BLOCKED_INSTALL_MESSAGE);
    let present = has_native_messaging && marker_matches;
    SynapseChromeSelfPolicyShieldStatus {
        present,
        detail: format!(
            "synapse_chrome_self_policy_shield_present={} policy_hive=HKCU policy_path=Software\\Policies\\Google\\Chrome value=ExtensionSettings blocked_permissions={} marker_matches={} reason={} {policy_write_access}",
            present,
            if blocked.is_empty() {
                "<none>".to_owned()
            } else {
                blocked.join(",")
            },
            marker_matches,
            if present {
                "native_messaging_self_shield_active"
            } else {
                "self_entry_incomplete"
            }
        ),
    }
}

#[cfg(windows)]
fn chrome_policy_set_value_access_status(subkey: &str) -> String {
    use windows::{
        Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, RegCloseKey, RegOpenKeyExW,
        },
        core::PCWSTR,
    };

    let subkey_wide = wide_null(subkey);
    let mut key = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_wide.as_ptr()),
            None,
            KEY_SET_VALUE,
            &raw mut key,
        )
    };
    if status != windows::Win32::Foundation::ERROR_SUCCESS {
        return format!(
            "policy_set_value_access=false policy_set_value_access_reason=reg_open_key_set_value_failed status={} expected_acl=admin_only_managed_policy_root remediation=do not weaken HKCU\\{subkey} ACL for non-elevated setup; run scripts\\synapse-setup.ps1 from an elevated PowerShell only when the optional Chrome ExtensionSettings policy shield must be applied; until then rely on live chrome.management suppression and fail-closed command gates",
            status.0
        );
    }

    let close_status = unsafe { RegCloseKey(key) };
    if close_status != windows::Win32::Foundation::ERROR_SUCCESS {
        tracing::warn!(
            code = "CHROME_POLICY_WRITE_ACCESS_REGCLOSE_FAILED",
            status = close_status.0,
            "RegCloseKey failed after Chrome ExtensionSettings write-access probe"
        );
    }
    "policy_set_value_access=true policy_set_value_access_reason=key_set_value_allowed".to_owned()
}

#[cfg(not(windows))]
fn chrome_policy_set_value_access_status(_subkey: &str) -> String {
    "policy_set_value_access=false policy_set_value_access_reason=non_windows".to_owned()
}

#[cfg(not(windows))]
fn synapse_chrome_self_policy_shield_status() -> SynapseChromeSelfPolicyShieldStatus {
    SynapseChromeSelfPolicyShieldStatus {
        present: false,
        detail: "synapse_chrome_self_policy_shield_present=false reason=non_windows".to_owned(),
    }
}

#[cfg(windows)]
fn read_hkcu_registry_string(subkey: &str, value_name: &str) -> Option<String> {
    use windows::{
        Win32::{
            Foundation::ERROR_SUCCESS,
            System::Registry::{
                HKEY_CURRENT_USER, REG_VALUE_TYPE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
                RegGetValueW,
            },
        },
        core::PCWSTR,
    };

    let subkey_wide = wide_null(subkey);
    let value_wide = wide_null(value_name);
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    let mut value_type = REG_VALUE_TYPE::default();
    let mut byte_len = 0_u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_wide.as_ptr()),
            PCWSTR(value_wide.as_ptr()),
            flags,
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
            flags,
            Some(&raw mut value_type),
            Some(buffer.as_mut_ptr().cast()),
            Some(&raw mut byte_len),
        )
    };
    if status != ERROR_SUCCESS {
        tracing::warn!(
            code = "CHROME_SELF_POLICY_SHIELD_REGISTRY_READ_FAILED",
            status = status.0,
            "failed to read Chrome ExtensionSettings policy value"
        );
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

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn format_external_chrome_popup_risks(rows: &[String]) -> String {
    let shown = rows.iter().take(8).cloned().collect::<Vec<_>>().join(" | ");
    let extra = rows.len().saturating_sub(8);
    let suffix = if extra == 0 {
        String::new()
    } else {
        format!(" | +{extra} more")
    };
    format!("{shown}{suffix}")
}

fn external_chrome_popup_risk_warning(rows: &[String]) -> String {
    if rows.is_empty() {
        return "external_chrome_popup_risk_warning=false risk_count=0".to_owned();
    }
    format!(
        "external_chrome_popup_risk_warning=true external_chrome_popup_risk_scope=external_unmanaged_noninterference risk_count={} external_chrome_popup_risk={} disposition=informational_only normal_bridge_debugger_permission=false normal_bridge_management_permission=false remediation=none; Synapse neither attaches the debugger on the human profile nor disables or reconfigures unrelated extensions",
        rows.len(),
        format_external_chrome_popup_risks(rows)
    )
}

fn external_chrome_popup_risk_host_unavailable_warning(rows: &[String]) -> String {
    if rows.is_empty() {
        return "external_chrome_popup_risk_warning=false risk_count=0".to_owned();
    }
    format!(
        "external_chrome_popup_risk_warning=true external_chrome_popup_risk_scope=external_unmanaged_noninterference risk_count={} external_chrome_popup_risk={} disposition=informational_only host_unavailable=true remediation=none; unrelated extensions are outside Synapse authority and do not gate the debugger-free normal bridge",
        rows.len(),
        format_external_chrome_popup_risks(rows)
    )
}

fn external_chrome_layout_infobar_warning(rows: &[String]) -> String {
    if rows.is_empty() {
        return "external_chrome_layout_infobar_risk_warning=false layout_risk_count=0".to_owned();
    }
    format!(
        "external_chrome_layout_infobar_risk_warning=true external_chrome_layout_infobar_risk_scope=external_automation_chrome_layout_shift layout_risk_count={} external_chrome_layout_infobar_risk={}",
        rows.len(),
        rows.join(";")
    )
}

fn ensure_normal_bridge_external_popup_suppressed(
    hwnd: i64,
    command_kind: &'static str,
) -> Result<(), ChromeDebuggerBridgeError> {
    let self_risks = synapse_chrome_self_profile_surfaces();
    let self_active_risks = synapse_chrome_self_active_popup_risks(&self_risks);
    let self_policy_shield = synapse_chrome_self_policy_shield_status();
    if !self_active_risks.is_empty() {
        tracing::error!(
            code = "CHROME_SELF_POPUP_RISK_WARNING",
            hwnd,
            command_kind,
            risk_count = self_active_risks.len(),
            synapse_chrome_bridge_permission_risk = %format_external_chrome_popup_risks(&self_active_risks),
            "normal Chrome bridge refusing tabs/scripting command while Synapse bridge profile still has active debugger/nativeMessaging permission"
        );
        return Err(ChromeDebuggerBridgeError {
            code: error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
            detail: format!(
                "normal Synapse Chrome Bridge refused command {command_kind:?} before queueing any Chrome tabs/scripting command; hwnd={hwnd} reason=Synapse Chrome Bridge profile still exposes active debugger or nativeMessaging permission synapse_chrome_bridge_permission_risk={} remediation=deploy the popup-free bridge and reload it through browser_debugger.reload_bridge, then verify the physical profile row no longer reports either active permission",
                format_external_chrome_popup_risks(&self_active_risks)
            ),
        });
    }
    if !self_risks.is_empty() && !self_policy_shield.present {
        // Chromium keeps removed permissions in the granted set after an
        // extension update. Granted-only residue is diagnostic; active/manifest
        // permissions and the live runtime debugger API readback are the popup
        // capability gates.
        tracing::warn!(
            code = "CHROME_SELF_GRANTED_PERMISSION_RESIDUE",
            hwnd,
            command_kind,
            risk_count = self_risks.len(),
            synapse_chrome_bridge_permission_risk = %format_external_chrome_popup_risks(&self_risks),
            synapse_chrome_self_policy_shield = %self_policy_shield.detail,
            "normal Chrome bridge continuing because Synapse self debugger/nativeMessaging residue is granted-only and the live popup-free bridge identity reports debugger_api_available=false"
        );
    }
    Ok(())
}

fn popup_risk_suppression_summary(value: &Option<Value>) -> String {
    let Some(value) = value else {
        return "not_reported".to_owned();
    };
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let management_available = value
        .get("management_available")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let hazard_count = value
        .get("hazard_count")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let disabled_count = value
        .get("disabled_count")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let remaining_hazard_count = value
        .get("remaining_hazard_count")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let failure_count = value
        .get("failure_count")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let remaining = compact_popup_risk_entries(value.get("remaining_hazards"));
    let failures = compact_popup_risk_entries(value.get("failures"));
    format!(
        "status={status} ok={ok} management_available={management_available} hazard_count={hazard_count} disabled_count={disabled_count} remaining_hazard_count={remaining_hazard_count} failure_count={failure_count} remaining={remaining} failures={failures}"
    )
}

fn compact_popup_risk_entries(value: Option<&Value>) -> String {
    let Some(entries) = value.and_then(Value::as_array) else {
        return "<none>".to_owned();
    };
    if entries.is_empty() {
        return "<none>".to_owned();
    }
    let shown = entries
        .iter()
        .take(8)
        .map(|entry| {
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unnamed>");
            let permissions = entry
                .get("hazard_permissions")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let error = entry.get("error").and_then(Value::as_str).unwrap_or("");
            if error.is_empty() {
                format!("{id}:{name}:{permissions}")
            } else {
                format!("{id}:{name}:{permissions}:error={error}")
            }
        })
        .collect::<Vec<_>>()
        .join("|");
    let extra = entries.len().saturating_sub(8);
    if extra == 0 {
        shown
    } else {
        format!("{shown}|+{extra} more")
    }
}

fn active_api_permissions(setting: &Value) -> Vec<String> {
    api_permissions(setting, &["active_permissions", "api"])
}

fn granted_api_permissions(setting: &Value) -> Vec<String> {
    api_permissions(setting, &["granted_permissions", "api"])
}

fn manifest_api_permissions(setting: &Value) -> Vec<String> {
    api_permissions(setting, &["manifest", "permissions"])
}

fn api_permissions(setting: &Value, path: &[&str]) -> Vec<String> {
    let mut cursor = setting;
    for segment in path {
        let Some(next) = cursor.get(*segment) else {
            return Vec::new();
        };
        cursor = next;
    }
    let mut permissions = cursor
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    permissions.sort();
    permissions.dedup();
    permissions
}

fn hazard_api_permissions<'a>(permissions: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut hazards = permissions
        .into_iter()
        .filter(|permission| *permission == "debugger" || *permission == "nativeMessaging")
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    hazards.sort();
    hazards.dedup();
    hazards
}

fn synapse_self_hazard_api_permissions<'a>(
    permissions: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut hazards = permissions
        .into_iter()
        .filter(|permission| *permission == "debugger" || *permission == "nativeMessaging")
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    hazards.sort();
    hazards.dedup();
    hazards
}

fn external_popup_risk_enabled(
    runtime_state: &ChromeExtensionRuntimeState,
    has_active_or_manifest_hazard: bool,
    has_granted_hazard: bool,
) -> bool {
    if !runtime_state.disable_reasons.is_empty() || runtime_state.state == Some(0) {
        return false;
    }
    if runtime_state.state == Some(1) {
        return has_active_or_manifest_hazard || has_granted_hazard;
    }
    // State can be absent in Secure Preferences for active unpacked/profile
    // rows. Do not treat granted-only residue as active, but an external
    // manifest/active debugger permission with no disable reason can still
    // produce Chrome's layout-shifting debugger infobar.
    has_active_or_manifest_hazard
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChromeExtensionRuntimeState {
    state: Option<i64>,
    active_bit: Option<bool>,
    disable_reasons: Vec<u64>,
    runtime_enabled: bool,
}

fn chrome_extension_runtime_state(setting: &Value) -> ChromeExtensionRuntimeState {
    let state = setting.get("state").and_then(Value::as_i64);
    let active_bit = setting.get("active_bit").and_then(Value::as_bool);
    let mut disable_reasons = setting
        .get("disable_reasons")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_u64).collect::<Vec<_>>())
        .unwrap_or_default();
    disable_reasons.sort_unstable();
    disable_reasons.dedup();
    // Chromium stores Extension::State in preferences (DISABLED=0, ENABLED=1).
    // Stale permission rows can survive without `state`; they are not a
    // concrete enabled-runtime signal and the live chrome.management bridge
    // readback is the stronger authority for enabled hazards.
    let runtime_enabled = state == Some(1) && disable_reasons.is_empty();
    ChromeExtensionRuntimeState {
        state,
        active_bit,
        disable_reasons,
        runtime_enabled,
    }
}

fn format_disable_reasons(disable_reasons: &[u64]) -> String {
    if disable_reasons.is_empty() {
        "[]".to_owned()
    } else {
        format!(
            "[{}]",
            disable_reasons
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

fn external_chrome_native_messaging_processes() -> Vec<String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let command_line = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            if !command_line.contains("chrome.nativeMessaging")
                || command_line.contains(EXTENSION_ID)
            {
                return None;
            }
            let extension_id = command_line
                .split("chrome-extension://")
                .nth(1)
                .and_then(|tail| tail.get(0..32))
                .filter(|candidate| {
                    candidate.len() == 32
                        && candidate
                            .chars()
                            .all(|character| ('a'..='p').contains(&character))
                })
                .unwrap_or("<unknown>");
            Some(format!(
                "native_messaging_process pid={} name={} extension_id={extension_id}",
                pid.as_u32(),
                process.name().to_string_lossy()
            ))
        })
        .collect()
}

fn external_chrome_layout_infobar_processes() -> Vec<String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let name = process.name().to_string_lossy();
            if !name.eq_ignore_ascii_case("chrome.exe") {
                return None;
            }
            let command_line = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            if command_line.is_empty() {
                return None;
            }
            let command_args = process_command_args(process);

            let has_automation_controlled = command_line.contains("--disable-blink-features")
                && command_line.contains("AutomationControlled");
            let has_remote_debugging_pipe = command_line.contains("--remote-debugging-pipe");
            let has_remote_debugging_port = command_line.contains("--remote-debugging-port");
            let has_silent_debugger =
                command_line.contains("--silent-debugger-extension-api");
            let has_ms_playwright_mcp = command_line.contains("ms-playwright-mcp");

            let mut reasons = Vec::new();
            if has_automation_controlled {
                reasons.push("unsupported_flag_disable_blink_features_automation_controlled");
            }
            if (has_remote_debugging_pipe || has_remote_debugging_port) && !has_silent_debugger {
                reasons.push("remote_debugging_without_silent_debugger_extension_api");
            }
            if has_ms_playwright_mcp && has_automation_controlled {
                reasons.push("headed_ms_playwright_mcp_layout_banner");
            }
            if reasons.is_empty() {
                return None;
            }

            let parent_pid = process
                .parent().map_or_else(|| "<unknown>".to_owned(), |parent| parent.as_u32().to_string());
            let user_data_dir = process_switch_arg_value(&command_args, "--user-data-dir");
            let user_data_dir_state = chrome_user_data_dir_state_label(user_data_dir.as_deref());
            let user_data_dir_display = user_data_dir
                .as_deref().map_or_else(|| "<missing>".to_owned(), quote_detail_value);
            let parent_chain = chrome_process_parent_chain(&system, process.parent());
            let owner_hint = chrome_layout_infobar_owner_hint(
                &command_line,
                user_data_dir.as_deref(),
                &parent_chain,
            );
            let repair_hint = chrome_layout_infobar_repair_hint(owner_hint);
            let command_line_len = command_line.len();
            let command_line_sha256 = sha256_hex_lower(command_line.as_bytes());
            Some(format!(
                "chrome_process pid={} parent_pid={} parent_chain={} name={} reasons={} user_data_dir={} user_data_dir_state={} owner_hint={} repair_hint={} has_remote_debugging_pipe={} has_remote_debugging_port={} has_silent_debugger_extension_api={} has_ms_playwright_mcp_dir={} command_metadata_policy=safe_display_v1 command_line_len={} command_line_sha256=sha256:{}",
                pid.as_u32(),
                parent_pid,
                parent_chain,
                name,
                reasons.join(","),
                user_data_dir_display,
                user_data_dir_state,
                owner_hint,
                repair_hint,
                has_remote_debugging_pipe,
                has_remote_debugging_port,
                has_silent_debugger,
                has_ms_playwright_mcp,
                command_line_len,
                command_line_sha256
            ))
        })
        .collect()
}

fn process_command_args(process: &sysinfo::Process) -> Vec<String> {
    process
        .cmd()
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect()
}

fn process_switch_arg_value(args: &[String], switch: &str) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        if is_process_switch_arg(arg, switch) {
            if let Some((_head, value)) = arg.split_once('=') {
                return Some(trim_process_arg_quotes(value).to_owned());
            }
            if let Some(value) = args.get(index + 1) {
                return Some(trim_process_arg_quotes(value).to_owned());
            }
        }
    }
    None
}

fn is_process_switch_arg(arg: &str, switch: &str) -> bool {
    let lower = trim_process_arg_quotes(arg).to_ascii_lowercase();
    let switch = switch.to_ascii_lowercase();
    lower == switch || lower.starts_with(&format!("{switch}="))
}

fn trim_process_arg_quotes(value: &str) -> &str {
    value.trim().trim_matches('"')
}

fn chrome_user_data_dir_state_label(path: Option<&str>) -> &'static str {
    let Some(path) = path else {
        return "missing";
    };
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "missing";
    }
    if chrome_layout_default_user_data_dir(trimmed) {
        "default_profile"
    } else {
        "dedicated_or_external"
    }
}

fn chrome_layout_default_user_data_dir(path: &str) -> bool {
    let Some(default_dir) = std::env::var_os("LOCALAPPDATA") else {
        return false;
    };
    let default_dir = PathBuf::from(default_dir)
        .join("Google")
        .join("Chrome")
        .join("User Data");
    let candidate = normalize_chrome_process_path(path);
    let default_dir = normalize_chrome_process_path(default_dir.to_string_lossy().as_ref());
    candidate == default_dir || candidate.starts_with(&format!("{default_dir}\\"))
}

fn normalize_chrome_process_path(path: &str) -> String {
    let path = trim_process_arg_quotes(path);
    let path = std::path::Path::new(path);
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn chrome_process_parent_chain(system: &sysinfo::System, parent: Option<sysinfo::Pid>) -> String {
    let mut current = parent;
    let mut seen = BTreeSet::new();
    let mut parts = Vec::new();
    for _ in 0..6 {
        let Some(pid) = current else {
            break;
        };
        if !seen.insert(pid.as_u32()) {
            parts.push("cycle".to_owned());
            break;
        }
        let Some(process) = system.process(pid) else {
            parts.push(format!("{}:<missing>", pid.as_u32()));
            break;
        };
        let name = process.name().to_string_lossy();
        parts.push(format!("{}:{}", pid.as_u32(), compact_process_name(&name)));
        current = process.parent();
    }
    if parts.is_empty() {
        "<none>".to_owned()
    } else {
        parts.join(">")
    }
}

fn compact_process_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn chrome_layout_infobar_owner_hint(
    command_line: &str,
    user_data_dir: Option<&str>,
    parent_chain: &str,
) -> &'static str {
    let command_lower = command_line.to_ascii_lowercase();
    let user_data_lower = user_data_dir.unwrap_or_default().to_ascii_lowercase();
    let parent_lower = parent_chain.to_ascii_lowercase();
    if user_data_lower.contains("synapse-cdp-profiles")
        || parent_lower.contains("synapse-mcp.exe")
        || parent_lower.contains("synapse-mcp")
    {
        "synapse_owned_or_spawned"
    } else if command_lower.contains("ms-playwright-mcp")
        || user_data_lower.contains("ms-playwright-mcp")
    {
        "ms_playwright_mcp_external"
    } else if command_lower.contains("playwright") || user_data_lower.contains("playwright") {
        "playwright_external"
    } else {
        "unknown_external"
    }
}

fn chrome_layout_infobar_repair_hint(owner_hint: &str) -> &'static str {
    match owner_hint {
        "synapse_owned_or_spawned" => "terminate_exact_synapse_owned_pid_tree_or_session_cleanup",
        "ms_playwright_mcp_external" | "playwright_external" => {
            "stop_external_playwright_mcp_chrome_or_relaunch_with_popup_safe_flags"
        }
        _ => "do_not_attach_or_target_until_owner_identified",
    }
}

fn quote_detail_value(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn sha256_hex_lower(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn sha256_file_hex_lower(path: &Path) -> Result<String, String> {
    std::fs::read(path)
        .map(|bytes| sha256_hex_lower(&bytes))
        .map_err(|error| error.to_string())
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ChromeDebuggerMouseButton {
    Left,
    Right,
    Middle,
}

impl ChromeDebuggerMouseButton {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerClickPoint {
    pub x: f64,
    pub y: f64,
    pub target_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerTypeResult {
    pub x: f64,
    pub y: f64,
    pub target_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerTypeActiveElementResult {
    pub target_id: String,
    pub tab_id: u32,
    pub chars_typed: u32,
    pub readback_backend: String,
    pub method: String,
    pub before_active_element: ChromeDebuggerActiveElement,
    pub after_active_element: ChromeDebuggerActiveElement,
    pub expected_value: Option<String>,
    #[serde(default)]
    pub events_dispatched: Vec<String>,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    pub extension_id: Option<String>,
}

/// Result of the background-safe `setFieldValue` bridge command (#1000/#717):
/// an in-page React-safe field REPLACE on the user's normal Chrome, with no
/// debugger attach and no OS foreground. `before_value`/`after_value` are the
/// raw in-page field values returned to the daemon for an exact Source-of-Truth
/// comparison; they are hashed before leaving the daemon and never logged raw.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerSetFieldValueResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub chars_requested: u32,
    pub readback_backend: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub frame_id: Option<i64>,
    #[serde(default)]
    pub frame_document_id: Option<String>,
    /// `selector` or `active_element`.
    #[serde(default)]
    pub resolved_by: String,
    /// Number of editable+visible nodes the selector matched (1 on success).
    #[serde(default)]
    pub match_count: u32,
    #[serde(default)]
    pub tag_name: String,
    #[serde(default)]
    pub is_editable: bool,
    #[serde(default)]
    pub before_value: Option<String>,
    #[serde(default)]
    pub after_value: Option<String>,
    #[serde(default)]
    pub expected_value: Option<String>,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    pub extension_id: Option<String>,
}

/// Result of the typed `pageContent` bridge command: serialized
/// `document.documentElement.outerHTML` from a normal Chrome tab without
/// debugger attach.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerPageContentResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub history_current_index: i64,
    #[serde(default)]
    pub history_entry_count: u32,
    #[serde(default)]
    pub html: String,
    #[serde(default)]
    pub html_len: usize,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub max_bytes: usize,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub frame_id: Option<i64>,
    #[serde(default)]
    pub frame_document_id: Option<String>,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    pub extension_id: Option<String>,
}

/// Result of the typed `setContent` bridge command: document replacement via
/// `document.open/write/close` plus same-tab readback, without debugger attach.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerSetContentResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub before_url: String,
    #[serde(default)]
    pub before_title: String,
    #[serde(default)]
    pub after_url: String,
    #[serde(default)]
    pub after_title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub history_current_index: i64,
    #[serde(default)]
    pub history_entry_count: u32,
    #[serde(default)]
    pub html_len: usize,
    #[serde(default)]
    pub seeded_url: String,
    #[serde(default)]
    pub seeded_from_url: String,
    #[serde(default)]
    pub seeded_reason: String,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub frame_id: Option<i64>,
    #[serde(default)]
    pub frame_document_id: Option<String>,
    #[serde(default)]
    pub frame_result_count: u32,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerAriaSnapshotNode {
    pub element_id: String,
    #[serde(default)]
    pub parent_element_id: Option<String>,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub children_count: u32,
    // #1558 credential-redaction metadata. The browser-side AX builder classifies
    // and redacts sensitive form-control values at source; these fields let the host
    // re-apply the same classification as defense-in-depth before the snapshot reaches
    // MCP output, daemon logs, transcripts, or persisted audit rows. Wire format is
    // snake_case (this struct does not use `rename_all`), matching the extension.
    #[serde(default)]
    pub input_type: Option<String>,
    #[serde(default)]
    pub autocomplete: Option<String>,
    #[serde(default)]
    pub redacted: bool,
    #[serde(default)]
    pub value_length: Option<usize>,
    #[serde(default)]
    pub value_hash: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerAriaSnapshotResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub root_element_id: Option<String>,
    #[serde(default)]
    pub snapshot: String,
    #[serde(default)]
    pub nodes: Vec<ChromeDebuggerAriaSnapshotNode>,
    #[serde(default)]
    pub node_count: usize,
    #[serde(default)]
    pub total_ax_nodes: u32,
    #[serde(default)]
    pub max_nodes: usize,
    #[serde(default)]
    pub max_depth: u32,
    #[serde(default)]
    pub truncated_by_max_nodes: bool,
    #[serde(default)]
    pub truncated_by_depth: bool,
    #[serde(default)]
    pub frame_tree_frame_count: u32,
    #[serde(default)]
    pub attached_frame_target_count: u32,
    #[serde(default)]
    pub blocked_frame_targets: Vec<String>,
    #[serde(default)]
    pub frame_snapshot_errors: Vec<String>,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChromeDebuggerAssertElementState {
    #[serde(default)]
    pub tag_name: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    #[serde(default)]
    pub is_visible: bool,
    #[serde(default)]
    pub is_enabled: bool,
    #[serde(default)]
    pub is_checked: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerAssertPollResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub match_count: usize,
    #[serde(default)]
    pub element_id: Option<String>,
    #[serde(default)]
    pub state: Option<ChromeDebuggerAssertElementState>,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerLocateElementsResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub match_count: usize,
    #[serde(default)]
    pub returned_count: usize,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub element_ids: Vec<String>,
    #[serde(default)]
    pub frame: Option<ChromeDebuggerLocatedFrame>,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    #[serde(default)]
    pub frame_result_count: u32,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerWaitForTextResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub condition_met: bool,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default)]
    pub polling_interval_ms: u64,
    #[serde(default)]
    pub poll_count: u64,
    #[serde(default)]
    pub observed_text_len: usize,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerWaitForLoadStateResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub condition_met: bool,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default)]
    pub polling_interval_ms: u64,
    #[serde(default)]
    pub poll_count: u64,
    #[serde(default)]
    pub event_count: u64,
    #[serde(default)]
    pub network_event_count: u64,
    #[serde(default)]
    pub max_in_flight_requests: usize,
    #[serde(default)]
    pub in_flight_requests: usize,
    #[serde(default)]
    pub network_idle_quiet_ms: u64,
    #[serde(default)]
    pub lifecycle_network_idle_seen: bool,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerWaitForUrlResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub url_pattern: String,
    #[serde(default)]
    pub match_kind: String,
    #[serde(default)]
    pub condition_met: bool,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default)]
    pub polling_interval_ms: u64,
    #[serde(default)]
    pub poll_count: u64,
    #[serde(default)]
    pub navigation_event_count: u64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerNetworkWaitEntry {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub resource_type: Option<String>,
    #[serde(default)]
    pub request_headers: Option<Value>,
    #[serde(default)]
    pub response_received: bool,
    #[serde(default)]
    pub response_url: Option<String>,
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(default)]
    pub status_text: Option<String>,
    #[serde(default)]
    pub response_headers: Option<Value>,
    #[serde(default)]
    pub response_timing: Option<Value>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub remote_ip_address: Option<String>,
    #[serde(default)]
    pub remote_port: Option<i64>,
    #[serde(default)]
    pub encoded_data_length: Option<f64>,
    #[serde(default)]
    pub loading_finished: bool,
    #[serde(default)]
    pub loading_failed: bool,
    #[serde(default)]
    pub failure_error_text: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerWaitForNetworkResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub wait_kind: String,
    #[serde(default)]
    pub url_pattern: Option<String>,
    #[serde(default)]
    pub match_kind: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(default)]
    pub resource_type: Option<String>,
    #[serde(default)]
    pub condition_met: bool,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default)]
    pub polling_interval_ms: u64,
    #[serde(default)]
    pub poll_count: u64,
    #[serde(default)]
    pub event_count: u64,
    #[serde(default)]
    pub total_buffered: usize,
    #[serde(default)]
    pub dropped: u64,
    #[serde(default)]
    pub matched_entry: Option<ChromeDebuggerNetworkWaitEntry>,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerWaitForSelectorResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub condition_met: bool,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default)]
    pub polling_interval_ms: u64,
    #[serde(default)]
    pub poll_count: u64,
    #[serde(default)]
    pub match_count: usize,
    #[serde(default)]
    pub returned_count: usize,
    #[serde(default)]
    pub visible_count: usize,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub element_id: Option<String>,
    #[serde(default)]
    pub frame: Option<ChromeDebuggerLocatedFrame>,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    #[serde(default)]
    pub frame_result_count: u32,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerInspectElementResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub element_id: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub element: Value,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    #[serde(default)]
    pub frame_id: Option<i64>,
    #[serde(default)]
    pub frame_document_id: Option<String>,
    #[serde(default)]
    pub frame_result_count: u32,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerScrollIntoViewResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub element_id: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub scroll: Value,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    #[serde(default)]
    pub frame_id: Option<i64>,
    #[serde(default)]
    pub frame_document_id: Option<String>,
    #[serde(default)]
    pub frame_result_count: u32,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

/// Readback from the typed normal-bridge `clock` command.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChromeDebuggerClockReadback {
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub now_ms: Option<u64>,
    #[serde(default)]
    pub pending_timer_count: Option<u64>,
    #[serde(default)]
    pub fired_timer_count: Option<u64>,
    #[serde(default)]
    pub last_timer_id: Option<u64>,
    #[serde(default)]
    pub next_timer_ms: Option<u64>,
    #[serde(default)]
    pub error_count: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Result of the typed `clock` bridge command: Playwright-style page clock
/// control in the current normal Chrome document without debugger attach.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerClockResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub operation: String,
    #[serde(default)]
    pub init_script_identifier: Option<String>,
    #[serde(default)]
    pub init_script_newly_added: bool,
    #[serde(default)]
    pub installed_at_unix_ms: u64,
    #[serde(default)]
    pub readback: ChromeDebuggerClockReadback,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub frame_id: Option<i64>,
    #[serde(default)]
    pub frame_document_id: Option<String>,
    #[serde(default)]
    pub frame_result_count: u32,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChromeDebuggerPageEventEntry {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub event_kind: String,
    #[serde(default)]
    pub target_id: String,
    #[serde(default)]
    pub target_type: Option<String>,
    #[serde(default)]
    pub target_attached: Option<bool>,
    #[serde(default)]
    pub page_target_id: Option<String>,
    #[serde(default)]
    pub opener_id: Option<String>,
    #[serde(default)]
    pub opener_frame_id: Option<String>,
    #[serde(default)]
    pub can_access_opener: Option<bool>,
    #[serde(default)]
    pub browser_context_id: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub worker_id: Option<String>,
    #[serde(default)]
    pub worker_type: Option<String>,
    #[serde(default)]
    pub worker_url: Option<String>,
    #[serde(default)]
    pub frame_id: Option<String>,
    #[serde(default)]
    pub parent_frame_id: Option<String>,
    #[serde(default)]
    pub loader_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub navigation_type: Option<String>,
    #[serde(default)]
    pub timestamp_s: Option<f64>,
    #[serde(default)]
    pub observed_at_unix_ms: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChromeDebuggerPageTargetSnapshot {
    #[serde(default)]
    pub target_id: String,
    #[serde(default)]
    pub target_type: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub opener_id: Option<String>,
    #[serde(default)]
    pub opener_frame_id: Option<String>,
    #[serde(default)]
    pub can_access_opener: bool,
    #[serde(default)]
    pub browser_context_id: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub attached: bool,
    #[serde(default)]
    pub destroyed: bool,
    #[serde(default)]
    pub first_seen_seq: u64,
    #[serde(default)]
    pub last_seen_seq: u64,
    #[serde(default)]
    pub first_seen_unix_ms: u64,
    #[serde(default)]
    pub last_seen_unix_ms: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChromeDebuggerWorkerSnapshot {
    #[serde(default)]
    pub worker_id: String,
    #[serde(default)]
    pub worker_type: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub attached: bool,
    #[serde(default)]
    pub destroyed: bool,
    #[serde(default)]
    pub first_seen_seq: u64,
    #[serde(default)]
    pub last_seen_seq: u64,
    #[serde(default)]
    pub first_seen_unix_ms: u64,
    #[serde(default)]
    pub last_seen_unix_ms: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChromeDebuggerPageEventsFilters {
    #[serde(default)]
    pub since_seq: Option<u64>,
    #[serde(default)]
    pub limit: usize,
    #[serde(default)]
    pub event_kind: Option<String>,
    #[serde(default)]
    pub worker_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerPageEventsResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub capture_newly_armed: bool,
    #[serde(default)]
    pub armed_at_unix_ms: u64,
    #[serde(default)]
    pub capacity: usize,
    #[serde(default)]
    pub next_cursor: u64,
    #[serde(default)]
    pub returned: usize,
    #[serde(default)]
    pub total_buffered: usize,
    #[serde(default)]
    pub dropped: u64,
    #[serde(default)]
    pub filters: ChromeDebuggerPageEventsFilters,
    #[serde(default)]
    pub entries: Vec<ChromeDebuggerPageEventEntry>,
    #[serde(default)]
    pub pages: Vec<ChromeDebuggerPageTargetSnapshot>,
    #[serde(default)]
    pub workers: Vec<ChromeDebuggerWorkerSnapshot>,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerNodeValue {
    pub value: String,
    pub target_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerOpenTabResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub chrome_window_focused: Option<bool>,
    #[serde(default)]
    pub chrome_window_state: String,
    #[serde(default)]
    pub chrome_window_selection_reason: String,
    #[serde(default)]
    pub chrome_window_candidate_count: u32,
    #[serde(default)]
    pub chrome_window_non_focused_count: u32,
    #[serde(default)]
    pub target_active: bool,
    #[serde(default)]
    pub target_highlighted: bool,
    pub target_type: String,
    pub url: String,
    pub title: String,
    pub target_attached: bool,
    pub target_count_before: u32,
    pub target_count_after: u32,
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerCloseTabResult {
    pub target_id: String,
    pub tab_id: u32,
    pub target_count_before: u32,
    pub target_count_after: u32,
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerTabTarget {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub index: i32,
    pub target_type: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub highlighted: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub target_attached: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerListTabsResult {
    pub extension_id: Option<String>,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub chrome_window_focused: Option<bool>,
    #[serde(default)]
    pub chrome_window_state: String,
    #[serde(default)]
    pub chrome_window_selection_reason: String,
    #[serde(default)]
    pub chrome_window_candidate_count: u32,
    #[serde(default)]
    pub chrome_window_non_focused_count: u32,
    pub target_count: u32,
    pub active_tab_count: u32,
    pub tabs: Vec<ChromeDebuggerTabTarget>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerTargetInfo {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub target_type: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub highlighted: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub active_element: Option<ChromeDebuggerActiveElement>,
    #[serde(default)]
    pub page_text: Option<ChromeDebuggerPageText>,
    #[serde(default)]
    pub page_vitals: Option<ChromeDebuggerPageVitals>,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerFrameEntry {
    #[serde(default)]
    pub frame_id: String,
    #[serde(default)]
    pub parent_frame_id: Option<String>,
    #[serde(default)]
    pub cdp_target_id: String,
    #[serde(default)]
    pub target_type: String,
    #[serde(default)]
    pub target_attached: Option<bool>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub security_origin: Option<String>,
    #[serde(default)]
    pub loader_id: Option<String>,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub sibling_index: u32,
    #[serde(default)]
    pub child_count: u32,
    #[serde(default)]
    pub is_out_of_process: bool,
    #[serde(default)]
    pub frame_element_id: Option<String>,
    #[serde(default)]
    pub frame_element_backend_node_id: Option<i64>,
    #[serde(default)]
    pub frame_element_cdp_target_id: Option<String>,
    #[serde(default)]
    pub frame_element_source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerLocatedFrame {
    #[serde(default)]
    pub resolved: bool,
    #[serde(default)]
    pub matched_frame_count: usize,
    #[serde(default)]
    pub frame_id: Option<String>,
    #[serde(default)]
    pub parent_frame_id: Option<String>,
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub is_out_of_process: bool,
    #[serde(default)]
    pub frame_element_id: Option<String>,
    #[serde(default)]
    pub frame_element_cdp_target_id: Option<String>,
    #[serde(default)]
    pub frame_element_source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerFramesResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub frame_count: usize,
    #[serde(default)]
    pub oopif_target_count: u32,
    #[serde(default)]
    pub attached_frame_target_count: u32,
    #[serde(default)]
    pub frames: Vec<ChromeDebuggerFrameEntry>,
    #[serde(default)]
    pub blocked_frame_targets: Vec<String>,
    #[serde(default)]
    pub frame_snapshot_errors: Vec<String>,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    #[serde(default)]
    pub target_candidate_count: u32,
    #[serde(default)]
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerDownloadsResult {
    #[serde(default)]
    pub extension_id: Option<String>,
    #[serde(default)]
    pub operation: String,
    #[serde(default)]
    pub items: Vec<ChromeDebuggerDownloadEntry>,
    #[serde(default)]
    pub selected_item: Option<ChromeDebuggerDownloadEntry>,
    #[serde(default)]
    pub returned: u32,
    #[serde(default)]
    pub event_count: u32,
    #[serde(default)]
    pub next_event_cursor: u64,
    #[serde(default)]
    pub events: Vec<ChromeDebuggerDownloadEvent>,
    #[serde(default)]
    pub condition_met: bool,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerDownloadEntry {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub final_url: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub filename_basename: String,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub start_time: String,
    #[serde(default)]
    pub end_time: String,
    #[serde(default)]
    pub estimated_end_time: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub can_resume: bool,
    #[serde(default)]
    pub danger: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub bytes_received: u64,
    #[serde(default)]
    pub total_bytes: i64,
    #[serde(default)]
    pub file_size: i64,
    #[serde(default)]
    pub exists: Option<bool>,
    #[serde(default)]
    pub incognito: bool,
    #[serde(default)]
    pub referrer: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerDownloadEvent {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub event_kind: String,
    #[serde(default)]
    pub timestamp_unix_ms: u64,
    #[serde(default)]
    pub download_id: i64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub final_url: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub filename_basename: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub danger: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub bytes_received: u64,
    #[serde(default)]
    pub total_bytes: i64,
    #[serde(default)]
    pub file_size: i64,
    #[serde(default)]
    pub delta: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChromeDebuggerLargestContentfulPaint {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub entry_type: String,
    #[serde(default)]
    pub start_time: f64,
    #[serde(default)]
    pub render_time: f64,
    #[serde(default)]
    pub load_time: f64,
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub element_tag_name: Option<String>,
    #[serde(default)]
    pub element_id: Option<String>,
    #[serde(default)]
    pub element_class_name: Option<String>,
    #[serde(default)]
    pub element_selector: Option<String>,
    #[serde(default)]
    pub element_text: Option<String>,
    #[serde(default)]
    pub element_current_src: Option<String>,
    #[serde(default)]
    pub element_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChromeDebuggerPageVitals {
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub readback_source: String,
    #[serde(default)]
    pub visibility_state: Option<String>,
    #[serde(default)]
    pub document_hidden: Option<bool>,
    #[serde(default)]
    pub ready_state: Option<String>,
    #[serde(default)]
    pub lcp_supported: Option<bool>,
    #[serde(default)]
    pub lcp_entry_count: usize,
    #[serde(default)]
    pub lcp: Option<ChromeDebuggerLargestContentfulPaint>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_detail: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChromeDebuggerPageText {
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub readback_source: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub text_len: usize,
    #[serde(default)]
    pub text_truncated: bool,
    #[serde(default)]
    pub max_chars: usize,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_detail: Option<String>,
    #[serde(default)]
    pub readback_scope: Option<String>,
    #[serde(default)]
    pub frame_count: usize,
    #[serde(default)]
    pub frame_text_available_count: usize,
    #[serde(default)]
    pub frame_text_nonempty_count: usize,
    #[serde(default)]
    pub frames: Vec<ChromeDebuggerFrameReadback>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChromeDebuggerFrameReadback {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub frame_id: Option<i64>,
    #[serde(default)]
    pub document_id: Option<String>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_detail: Option<String>,
    #[serde(default)]
    pub matched_count: Option<usize>,
    #[serde(default)]
    pub resolved_by: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub ready_state: Option<String>,
    #[serde(default)]
    pub has_active_element: Option<bool>,
    #[serde(default)]
    pub is_editable: Option<bool>,
    #[serde(default)]
    pub tag_name: Option<String>,
    #[serde(default)]
    pub text_len: Option<usize>,
    #[serde(default)]
    pub text_truncated: Option<bool>,
    #[serde(default)]
    pub text_sample: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChromeDebuggerActiveElement {
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub readback_source: String,
    #[serde(default)]
    pub has_active_element: Option<bool>,
    #[serde(default)]
    pub is_editable: Option<bool>,
    #[serde(default)]
    pub tag_name: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub selected_text: Option<String>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_detail: Option<String>,
    #[serde(default)]
    pub frame_id: Option<i64>,
    #[serde(default)]
    pub frame_document_id: Option<String>,
    #[serde(default)]
    pub frame_result_count: usize,
    #[serde(default)]
    pub frame_results: Vec<ChromeDebuggerFrameReadback>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerNavigateResult {
    pub target_id: String,
    pub tab_id: u32,
    pub action: String,
    pub requested_url: Option<String>,
    pub before_url: String,
    pub before_title: String,
    pub after_url: String,
    pub after_title: String,
    pub ready_state: String,
    pub history_current_index: i64,
    pub history_entry_count: u32,
    pub history_readback_source: String,
    pub readback_backend: String,
    pub navigation_error_text: Option<String>,
    pub is_download: Option<bool>,
    #[serde(default)]
    pub navigation_claim_id: Option<String>,
    #[serde(default)]
    pub navigation_correlation_status: Option<String>,
    #[serde(default)]
    pub initial_document_id: Option<String>,
    #[serde(default)]
    pub final_document_id: Option<String>,
    /// #1344: structured download outcome when the navigate started a Chrome
    /// download instead of changing the tab URL.
    #[serde(default)]
    pub download_status: Option<String>,
    #[serde(default)]
    pub download_id: Option<i64>,
    #[serde(default)]
    pub download_url: Option<String>,
    #[serde(default)]
    pub download_final_url: Option<String>,
    #[serde(default)]
    pub download_filename: Option<String>,
    #[serde(default)]
    pub download_state: Option<String>,
    #[serde(default)]
    pub download_match_reason: Option<String>,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeDebuggerActivateTabResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub before_active: Option<bool>,
    pub active: bool,
    #[serde(default)]
    pub highlighted: Option<bool>,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    pub readback_backend: String,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeBridgeReloadCommandAck {
    pub ok: bool,
    pub control_surface: String,
    pub required_foreground: bool,
    pub installer_path: String,
    pub installer_sha256: String,
    pub installer_exit_code: i32,
    pub installer_stdout_sha256: String,
    pub installer_stderr_sha256: String,
    pub installer_duration_ms: u64,
    pub extension_id: String,
    pub extension_dir: String,
    pub extension_service_worker_sha256: String,
    pub active_profile: String,
    pub reason: String,
    pub profile_before_installed: bool,
    pub profile_before_ready: bool,
    pub profile_after_installed: bool,
    pub profile_after_ready: bool,
    pub loaded_build_id: String,
    pub deployed_build_id: String,
    pub scheduled_at_unix_ms: u64,
    pub reload_delay_ms: u64,
    pub foreground_api_calls: u64,
    pub tab_mutations: u64,
    pub synthetic_input_events: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChromeBridgeRuntimeReloadAck {
    ok: bool,
    control_surface: String,
    required_foreground: bool,
    extension_id: String,
    loaded_build_id: String,
    deployed_build_id: String,
    scheduled_at_unix_ms: u64,
    reload_delay_ms: u64,
    foreground_api_calls: u64,
    tab_mutations: u64,
    synthetic_input_events: u64,
}

#[derive(Debug)]
struct ChromeBridgeBackgroundDeployReadback {
    installer_path: String,
    installer_sha256: String,
    installer_exit_code: i32,
    installer_stdout_sha256: String,
    installer_stderr_sha256: String,
    installer_duration_ms: u64,
    extension_dir: String,
    extension_service_worker_sha256: String,
    active_profile: String,
    profile_before_installed: bool,
    profile_before_ready: bool,
    profile_after_installed: bool,
    profile_after_ready: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeBridgeHostSnapshot {
    pub host_id: String,
    pub origin: String,
    pub extension_id: Option<String>,
    pub extension_version: Option<String>,
    pub extension_protocol_version: Option<u32>,
    pub extension_build_id: Option<String>,
    pub extension_build_sha256: Option<String>,
    pub extension_declared_build_sha256: Option<String>,
    pub extension_service_worker_sha256: Option<String>,
    pub extension_service_worker_sha256_status: Option<String>,
    pub extension_service_worker_sha256_source: Option<String>,
    pub extension_service_worker_byte_length: Option<u64>,
    pub extension_service_worker_sha256_error: Option<String>,
    pub expected_service_worker_sha256: Option<String>,
    pub expected_service_worker_path: Option<String>,
    pub extension_capabilities: Vec<String>,
    pub extension_user_agent: Option<String>,
    pub extension_debugger_api_available: Option<bool>,
    pub extension_popup_risk_suppression: Option<Value>,
    pub extension_startup_readback: Option<Value>,
    pub pid: u32,
    pub parent_window: Option<String>,
    pub transport: Option<String>,
    pub registered_unix_ms: u64,
    pub last_seen_unix_ms: u64,
    pub last_disconnect_detail: Option<String>,
    pub last_detach_reason: Option<String>,
    pub extension_stale: bool,
    pub extension_stale_reasons: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeBridgeReloadResult {
    pub before: Option<ChromeBridgeHostSnapshot>,
    pub command_ack: ChromeBridgeReloadCommandAck,
    pub after: ChromeBridgeHostSnapshot,
    pub reconnected: bool,
    pub waited_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeBridgeMaintenancePauseAck {
    pub ok: bool,
    pub host_id: Option<String>,
    pub pause_ms: u64,
    pub pause_until_unix_ms: u64,
    pub resume_probe_after_ms: u64,
    pub resume_probe_after_unix_ms: u64,
    #[serde(default)]
    pub paused_daemon_pid: Option<u32>,
    #[serde(default)]
    pub paused_daemon_instance_id: Option<String>,
    pub reason: String,
    pub reconnect_suppressed: bool,
    pub persisted: bool,
    pub websocket_close: ChromeBridgeWebSocketCloseAck,
    pub bridge_build_id: Option<String>,
    pub extension_id: Option<String>,
    #[serde(default)]
    pub daemon_disconnect_requested: bool,
    #[serde(default)]
    pub daemon_disconnect_host_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChromeBridgeWebSocketCloseAck {
    pub had_socket: bool,
    pub ready_state_before: Option<u16>,
    pub close_requested: bool,
    #[serde(default)]
    pub close_deferred: bool,
    pub close_error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct NativeRegisterResponse {
    ok: bool,
    host_id: String,
    bridge_token: String,
    bridge_protocol_version: u32,
    native_host_name: String,
    expected_extension_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct NativeRegisterRequest {
    origin: String,
    pid: u32,
    parent_window: Option<String>,
    bridge_protocol_version: u32,
    transport: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct NativeMessageRequest {
    host_id: String,
    message: Value,
}

#[derive(Debug, Deserialize)]
pub struct NativeMaintenancePauseRequest {
    pause_ms: Option<u64>,
    reason: String,
    resume_probe_after_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct NativeReconnectProbeResponse {
    ok: bool,
    daemon_pid: u32,
    daemon_instance_id: String,
    daemon_now_unix_ms: u64,
    bridge_protocol_version: u32,
    expected_extension_id: String,
    expected_extension_build_id: String,
}

#[derive(Debug, Deserialize)]
pub struct NativeNextQuery {
    host_id: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct NativeWsQuery {
    host_id: String,
    bridge_token: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct NativeNextResponse {
    ok: bool,
    command: Option<ChromeCommand>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChromeCommand {
    id: String,
    kind: String,
    params: Value,
}

#[derive(Clone, Debug, Deserialize)]
struct ChromeResponse {
    id: String,
    ok: bool,
    result: Option<Value>,
    error: Option<ChromeResponseError>,
}

#[derive(Clone, Debug, Deserialize)]
struct ChromeResponseError {
    code: Option<String>,
    detail: Option<String>,
    diagnostics: Option<ChromeResponseErrorDiagnostics>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChromeResponseErrorDiagnostics {
    exception_type: String,
    exception_message: String,
    line_number: Option<u64>,
    column_number: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CommandTerminalEnvelope {
    #[serde(rename = "type")]
    message_type: String,
    protocol_version: u32,
    original_host_id: String,
    browser_session_id: String,
    command_id: String,
    command_kind: String,
    terminal_sequence: u64,
    payload_json: String,
    payload_sha256: String,
    payload_bytes: usize,
    created_at_unix_ms: u64,
    worker_boot_id: String,
}

#[derive(Clone, Debug)]
struct CommandTerminalReceipt {
    command_id: String,
    command_kind: String,
    original_host_id: String,
    browser_session_id: String,
    terminal_sequence: u64,
    payload_sha256: String,
    payload_bytes: usize,
}

#[derive(Clone, Debug, Deserialize)]
struct ExtensionTabNavigationEvent {
    #[serde(default)]
    event_id: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    target_id: String,
    tab_id: u32,
    #[serde(default)]
    chrome_window_id: Option<i64>,
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    highlighted: bool,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    initiator: String,
    #[serde(default)]
    claim_id: Option<String>,
    #[serde(default)]
    claim_status: Option<String>,
    #[serde(default)]
    agent_session_id: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    requested_url: Option<String>,
    #[serde(default)]
    before_url: Option<String>,
    #[serde(default)]
    before_document_id: Option<String>,
    #[serde(default)]
    correlation_verdict: String,
    #[serde(default)]
    frame_id: Option<i64>,
    #[serde(default)]
    parent_frame_id: Option<i64>,
    #[serde(default)]
    document_id: Option<String>,
    #[serde(default)]
    parent_document_id: Option<String>,
    #[serde(default)]
    document_lifecycle: Option<String>,
    #[serde(default)]
    frame_type: Option<String>,
    #[serde(default)]
    transition_type: Option<String>,
    #[serde(default)]
    transition_qualifiers: Vec<String>,
    #[serde(default)]
    navigation_error: Option<String>,
    #[serde(default)]
    navigation_timestamp_ms: Option<u64>,
    #[serde(default)]
    observed_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ChromeDebuggerBrowserNavigationEvent {
    pub event_id: String,
    pub source: String,
    pub event: String,
    pub url: String,
    pub title: String,
    pub tab_id: Option<u32>,
    pub chrome_window_id: Option<i64>,
    pub cdp_target_id: Option<String>,
    pub endpoint: Option<String>,
    pub transport: Option<String>,
    pub ready_state: Option<String>,
    pub observed_at_unix_ms: Option<u64>,
    pub active: Option<bool>,
    pub highlighted: Option<bool>,
    pub pinned: Option<bool>,
    pub initiator: String,
    pub claim_id: Option<String>,
    pub claim_status: Option<String>,
    pub agent_session_id: Option<String>,
    pub action: Option<String>,
    pub requested_url: Option<String>,
    pub before_url: Option<String>,
    pub before_document_id: Option<String>,
    pub correlation_verdict: String,
    pub frame_id: Option<i64>,
    pub parent_frame_id: Option<i64>,
    pub document_id: Option<String>,
    pub parent_document_id: Option<String>,
    pub document_lifecycle: Option<String>,
    pub frame_type: Option<String>,
    pub transition_type: Option<String>,
    pub transition_qualifiers: Vec<String>,
    pub navigation_error: Option<String>,
    pub navigation_timestamp_ms: Option<u64>,
}

struct PendingResponse {
    host_id: String,
    kind: String,
    sender: Option<oneshot::Sender<ChromeResponse>>,
    mutation_capable: bool,
    delivered: bool,
    caller_timed_out: bool,
    transport_lost: bool,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct ChromeDebuggerMutationOwnersReadback {
    pub queued_mutation_count: usize,
    pub delivered_mutation_count: usize,
    pub delivered_caller_timed_out_count: usize,
    pub delivered_transport_lost_count: usize,
    pub delivered_command_ids: Vec<String>,
    pub registry_readback_healthy: bool,
    pub registry_readback_failure: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct ChromeDebuggerMutationOwnersDrainReadback {
    pub queued_mutations_canceled: usize,
    pub terminal_acks_observed: usize,
    pub failures: Vec<String>,
    pub readback: ChromeDebuggerMutationOwnersReadback,
    pub fully_drained: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChromeDebuggerExtensionOwnerCounts {
    pub opened_tab_count: usize,
    pub init_script_count: usize,
    pub binding_count: usize,
    pub debugger_attached_tab_count: usize,
    pub unresolved_debugger_command_timeout_count: usize,
    pub executed_init_script_effect_unresolved_count: usize,
    pub dialog_policy_count: usize,
    pub dialog_auto_handle_in_flight_count: usize,
    pub file_chooser_interception_count: usize,
    pub clock_count: usize,
    pub viewport_override_count: usize,
    pub device_override_count: usize,
    pub geolocation_override_count: usize,
    pub locale_override_count: usize,
    pub media_override_count: usize,
    pub network_override_count: usize,
    #[serde(default)]
    pub command_terminal_pending_count: usize,
    #[serde(default)]
    pub capture_visible_tab_pending_count: usize,
    #[serde(default)]
    pub capture_visible_tab_quarantined_count: usize,
    pub mutation_handler_in_flight_count: usize,
}

impl ChromeDebuggerExtensionOwnerCounts {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.opened_tab_count == 0
            && self.init_script_count == 0
            && self.binding_count == 0
            && self.debugger_attached_tab_count == 0
            && self.unresolved_debugger_command_timeout_count == 0
            && self.executed_init_script_effect_unresolved_count == 0
            && self.dialog_policy_count == 0
            && self.dialog_auto_handle_in_flight_count == 0
            && self.file_chooser_interception_count == 0
            && self.clock_count == 0
            && self.viewport_override_count == 0
            && self.device_override_count == 0
            && self.geolocation_override_count == 0
            && self.locale_override_count == 0
            && self.media_override_count == 0
            && self.network_override_count == 0
            && self.command_terminal_pending_count == 0
            && self.capture_visible_tab_pending_count == 0
            && self.capture_visible_tab_quarantined_count == 0
            && self.mutation_handler_in_flight_count == 0
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChromeDebuggerExtensionOwnerReadback {
    pub enabled: bool,
    pub disable_sequence: u64,
    pub command_activity_sequence: u64,
    pub command_last_completed_sequence: u64,
    pub command_in_flight_count: usize,
    pub mutation_handlers_started_count: u64,
    pub mutation_handlers_completed_count: u64,
    pub worker_boot_id: String,
    pub browser_session_id: Option<String>,
    pub ledger_browser_session_id: String,
    pub browser_session_continuity_matched: bool,
    pub stale_browser_session_owner_count: usize,
    pub storage_state_loaded: bool,
    pub storage_state_load_error: Option<String>,
    #[serde(default)]
    pub storage_state_failure_diagnostic: Option<serde_json::Value>,
    #[serde(default)]
    pub persisted_schema_version: u64,
    pub persisted_state_revision: u64,
    #[serde(default)]
    pub schema_5_migration: Option<serde_json::Value>,
    pub persisted_in_flight_mutation: Option<serde_json::Value>,
    #[serde(default)]
    pub command_terminal_outbox: Vec<serde_json::Value>,
    #[serde(default)]
    pub last_command_terminal_ack: Option<serde_json::Value>,
    #[serde(default)]
    pub capture_visible_tab_lease: Option<serde_json::Value>,
    #[serde(default)]
    pub last_capture_visible_tab_settlement: Option<serde_json::Value>,
    pub unresolved_debugger_command_timeouts:
        Vec<ChromeDebuggerExtensionUnresolvedDebuggerCommandTimeout>,
    pub unresolved_worker_restart_mutation_count: usize,
    pub owner_continuity_healthy: bool,
    pub active_after: ChromeDebuggerExtensionOwnerCounts,
    pub fully_drained: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChromeDebuggerExtensionUnresolvedDebuggerCommandTimeout {
    pub id: String,
    pub tab_id: i64,
    pub method: String,
    pub activity_sequence: u64,
    pub worker_boot_id: String,
    pub timed_out_at_unix_ms: u64,
    pub neutralization_method: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChromeDebuggerExtensionClosedOpenedTab {
    pub tab_id: i64,
    pub target_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChromeDebuggerExtensionReloadedInitScriptEffectTab {
    pub tab_id: i64,
    pub before_document_id: String,
    pub after_document_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChromeDebuggerExtensionCleanupReadback {
    #[serde(flatten)]
    pub owner: ChromeDebuggerExtensionOwnerReadback,
    pub expected_disable_sequence: u64,
    #[serde(default)]
    pub opened_tabs_found: usize,
    #[serde(default)]
    pub opened_tabs_closed: usize,
    #[serde(default)]
    pub closed_opened_tabs: Vec<ChromeDebuggerExtensionClosedOpenedTab>,
    #[serde(default)]
    pub remaining_opened_tabs: Vec<ChromeDebuggerExtensionClosedOpenedTab>,
    #[serde(default)]
    pub init_scripts_found: usize,
    #[serde(default)]
    pub init_scripts_removed: usize,
    #[serde(default)]
    pub init_script_effects_found: usize,
    #[serde(default)]
    pub init_script_effects_cleared: usize,
    #[serde(default)]
    pub reloaded_init_script_effect_tabs: Vec<ChromeDebuggerExtensionReloadedInitScriptEffectTab>,
    #[serde(default)]
    pub bindings_found: usize,
    #[serde(default)]
    pub bindings_removed: usize,
    #[serde(default)]
    pub debugger_tabs_found: usize,
    #[serde(default)]
    pub debugger_tabs_detached: usize,
    #[serde(default)]
    pub detached_debugger_tabs: Vec<i64>,
    #[serde(default)]
    pub dialog_policies_found: usize,
    #[serde(default)]
    pub dialog_policies_disabled: usize,
    #[serde(default)]
    pub file_chooser_interceptions_found: usize,
    #[serde(default)]
    pub file_chooser_interceptions_disabled: usize,
    #[serde(default)]
    pub clocks_found: usize,
    #[serde(default)]
    pub clocks_uninstalled: usize,
    #[serde(default)]
    pub failures: Vec<String>,
}

/// The parts of a bridge registration whose change is worth an operator's
/// attention.
///
/// A `hello` that reproduces all of these exactly is a re-registration that
/// changed nothing. The reconnect wake alarm produced ~2,100 of those a day on
/// this host, 3.3% of the daemon's entire log, which is enough volume to hide a
/// genuine bridge fault (#1880). Capabilities and both service-worker digests
/// are included because a silent change to any of them is precisely the event
/// the HELLO line exists to announce.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ChromeBridgeHelloIdentity {
    extension_id: Option<String>,
    extension_version: Option<String>,
    protocol_version: Option<u32>,
    build_id: Option<String>,
    declared_build_sha256: Option<String>,
    service_worker_sha256: Option<String>,
    service_worker_sha256_status: Option<String>,
    running_service_worker_sha256: Option<String>,
    running_service_worker_sha256_status: Option<String>,
    debugger_api_available: Option<bool>,
    capabilities: BTreeSet<String>,
    transport: Option<String>,
}

impl ChromeBridgeHelloIdentity {
    fn of(host: &HostRecord) -> Self {
        Self {
            extension_id: host.extension_id.clone(),
            extension_version: host.extension_version.clone(),
            protocol_version: host.extension_protocol_version,
            build_id: host.extension_build_id.clone(),
            declared_build_sha256: host.extension_declared_build_sha256.clone(),
            service_worker_sha256: host.extension_service_worker_sha256.clone(),
            service_worker_sha256_status: host.extension_service_worker_sha256_status.clone(),
            running_service_worker_sha256: host.extension_running_service_worker_sha256.clone(),
            running_service_worker_sha256_status: host
                .extension_running_service_worker_sha256_status
                .clone(),
            debugger_api_available: host.extension_debugger_api_available,
            capabilities: host.extension_capabilities.clone(),
            transport: host.transport.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChromeDebuggerExtensionEnableReadback {
    #[serde(flatten)]
    pub owner: ChromeDebuggerExtensionOwnerReadback,
    pub expected_disable_sequence: u64,
    pub generation_matched: bool,
    pub owners_were_empty: bool,
    pub enable_applied: bool,
}

#[derive(Clone, Debug)]
struct HostRecord {
    origin: String,
    extension_id: Option<String>,
    extension_version: Option<String>,
    extension_protocol_version: Option<u32>,
    extension_build_id: Option<String>,
    extension_build_sha256: Option<String>,
    extension_declared_build_sha256: Option<String>,
    extension_service_worker_sha256: Option<String>,
    extension_service_worker_sha256_status: Option<String>,
    extension_service_worker_sha256_source: Option<String>,
    extension_service_worker_byte_length: Option<u64>,
    extension_service_worker_sha256_error: Option<String>,
    /// Fingerprint of the bytes the worker is actually EXECUTING, captured at
    /// its startup. `extension_service_worker_sha256` above is the worker's
    /// read of the file on disk *now*, which a redeploy changes without
    /// changing the running code — comparing that against the daemon's own
    /// on-disk read compared a value with itself and reported freshness for a
    /// worker running old code (#1828).
    extension_running_service_worker_sha256: Option<String>,
    extension_running_service_worker_sha256_status: Option<String>,
    extension_capabilities: BTreeSet<String>,
    extension_user_agent: Option<String>,
    extension_debugger_api_available: Option<bool>,
    extension_popup_risk_suppression: Option<Value>,
    extension_startup_readback: Option<Value>,
    pid: u32,
    parent_window: Option<String>,
    transport: Option<String>,
    bridge_token_digest: [u8; 32],
    registered_unix_ms: u64,
    last_seen_unix_ms: u64,
    last_disconnect_detail: Option<String>,
    last_detach_reason: Option<String>,
}

#[derive(Clone, Debug)]
struct ChromeBridgeHealthRecord {
    host_id: String,
    origin: String,
    extension_id: Option<String>,
    extension_version: Option<String>,
    extension_protocol_version: Option<u32>,
    extension_build_id: Option<String>,
    extension_build_sha256: Option<String>,
    extension_declared_build_sha256: Option<String>,
    extension_service_worker_sha256: Option<String>,
    extension_service_worker_sha256_status: Option<String>,
    extension_service_worker_sha256_source: Option<String>,
    extension_service_worker_byte_length: Option<u64>,
    extension_service_worker_sha256_error: Option<String>,
    expected_service_worker_sha256: Option<String>,
    expected_service_worker_path: Option<String>,
    extension_capabilities: BTreeSet<String>,
    extension_user_agent: Option<String>,
    extension_debugger_api_available: Option<bool>,
    extension_popup_risk_suppression: Option<Value>,
    extension_startup_readback: Option<Value>,
    pid: u32,
    parent_window: Option<String>,
    transport: Option<String>,
    registered_unix_ms: u64,
    last_seen_unix_ms: u64,
    last_disconnect_detail: Option<String>,
    last_detach_reason: Option<String>,
}

struct QueuedCommand {
    host_id: String,
    command: ChromeCommand,
}

#[derive(Default)]
struct BridgeInner {
    active_host_id: Option<String>,
    host_lineage_sequence: u64,
    hosts: HashMap<String, HostRecord>,
    commands: VecDeque<QueuedCommand>,
    pending: HashMap<String, PendingResponse>,
    command_terminal_receipts: VecDeque<CommandTerminalReceipt>,
    command_terminal_high_water: HashMap<String, u64>,
}

#[derive(Copy, Clone, Debug, Default)]
struct TransportLossPendingStats {
    queued_removed: usize,
    pending_failed_before_delivery: usize,
    delivered_commands_preserved: usize,
}

fn preserve_delivered_mutations_on_transport_loss(
    inner: &mut BridgeInner,
    host_id: &str,
    detail: &str,
) -> TransportLossPendingStats {
    let queued_before = inner.commands.len();
    inner.commands.retain(|queued| queued.host_id != host_id);
    let queued_removed = queued_before.saturating_sub(inner.commands.len());
    let pending_ids = inner
        .pending
        .iter()
        .filter(|(_id, pending)| pending.host_id == host_id)
        .map(|(id, _pending)| id.clone())
        .collect::<Vec<_>>();
    let mut pending_failed_before_delivery = 0usize;
    let mut delivered_commands_preserved = 0usize;
    for id in pending_ids {
        let preserve = inner
            .pending
            .get(&id)
            .is_some_and(|pending| pending.delivered);
        if preserve {
            if let Some(pending) = inner.pending.get_mut(&id) {
                pending.transport_lost = true;
                delivered_commands_preserved = delivered_commands_preserved.saturating_add(1);
                tracing::warn!(
                    code = "CHROME_DEBUGGER_DELIVERED_COMMAND_TRANSPORT_LOST",
                    host_id = %host_id,
                    command_id = %id,
                    command_kind = %pending.kind,
                    detail = %detail,
                    "delivered Chrome command remains owned while its durable terminal is reconciled on a replacement authenticated WebSocket"
                );
            }
        } else if let Some(pending) = inner.pending.remove(&id) {
            if let Some(sender) = pending.sender {
                let _ = sender.send(ChromeResponse {
                    id: id.clone(),
                    ok: false,
                    result: None,
                    error: Some(ChromeResponseError {
                        code: Some(error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE.to_owned()),
                        detail: Some(detail.to_owned()),
                        diagnostics: None,
                    }),
                });
            }
            pending_failed_before_delivery = pending_failed_before_delivery.saturating_add(1);
        }
    }
    TransportLossPendingStats {
        queued_removed,
        pending_failed_before_delivery,
        delivered_commands_preserved,
    }
}

struct ChromeDebuggerBridge {
    inner: Mutex<BridgeInner>,
    notify: Notify,
    command_seq: AtomicU64,
}

type BrowserNavigationSink = dyn Fn(ChromeDebuggerBrowserNavigationEvent) + Send + Sync + 'static;

fn browser_navigation_sink_slot() -> &'static Mutex<Option<Arc<BrowserNavigationSink>>> {
    static SINK: OnceLock<Mutex<Option<Arc<BrowserNavigationSink>>>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(None))
}

pub fn set_browser_navigation_sink(sink: Arc<BrowserNavigationSink>) {
    match browser_navigation_sink_slot().lock() {
        Ok(mut guard) => *guard = Some(sink),
        Err(poisoned) => *poisoned.into_inner() = Some(sink),
    }
}

fn emit_browser_navigation_event(event: ChromeDebuggerBrowserNavigationEvent) {
    let sink = match browser_navigation_sink_slot().lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    if let Some(sink) = sink {
        sink(event);
    } else {
        tracing::warn!(
            code = "CHROME_DEBUGGER_BROWSER_NAV_SINK_MISSING",
            "Chrome debugger browser navigation event had no recorder sink"
        );
    }
}

fn string_set_field(value: &Value, field: &str) -> BTreeSet<String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn format_capabilities(capabilities: &BTreeSet<String>) -> String {
    if capabilities.is_empty() {
        "<none>".to_owned()
    } else {
        capabilities
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn bridge_missing_required_capabilities(capabilities: &BTreeSet<String>) -> Vec<String> {
    REQUIRED_DIRECT_HTTP_CAPABILITIES
        .iter()
        .copied()
        .filter(|capability| !capabilities.contains(*capability))
        .map(str::to_owned)
        .collect()
}

fn bridge_command_stale_reason(host: &HostRecord, kind: &str) -> Option<String> {
    let profile_install_state = synapse_chrome_profile_install_state();
    bridge_command_stale_reason_with_profile_state(host, kind, &profile_install_state)
}

fn bridge_command_stale_reason_with_profile_state(
    host: &HostRecord,
    kind: &str,
    profile_install_state: &SynapseChromeProfileInstallState,
) -> Option<String> {
    if host.transport.as_deref() != Some("direct_http") {
        return None;
    }
    if host.extension_id.as_deref() != Some(EXTENSION_ID) {
        return Some(format!(
            "extension_id_mismatch actual={} expected={EXTENSION_ID}",
            host.extension_id.as_deref().unwrap_or("not_seen_yet")
        ));
    }
    if kind == MAINTENANCE_RECONNECT_PAUSE_COMMAND || kind == BACKGROUND_RUNTIME_RELOAD_COMMAND {
        if host.extension_capabilities.contains(kind) {
            return None;
        }
        return Some(format!(
            "missing_capability={kind} loaded_capabilities={}",
            format_capabilities(&host.extension_capabilities)
        ));
    }
    // Compare the daemon's on-disk read against the bytes the worker is
    // EXECUTING, not against the worker's own re-read of the same file. The
    // latter is the same value from a second source, so it always matched and
    // reported "not stale" for a worker still running pre-redeploy code
    // (#1828). A bridge too old to report its running identity falls back to
    // the on-disk value — degraded, but never worse than the old behavior.
    let running_sha256 = host
        .extension_running_service_worker_sha256
        .as_deref()
        .or(host.extension_service_worker_sha256.as_deref());
    let running_status = host
        .extension_running_service_worker_sha256_status
        .as_deref()
        .or(host.extension_service_worker_sha256_status.as_deref());
    let mut identity_reasons = Vec::new();
    if let Some(reason) = bridge_build_id_stale_reason(
        host.extension_build_id.as_deref(),
        EXPECTED_EXTENSION_BUILD_ID,
        running_sha256,
        profile_install_state
            .active_profile_service_worker_sha256
            .as_deref(),
        running_status,
    ) {
        identity_reasons.push(reason);
    }
    if let Some(reason) = service_worker_integrity_stale_reason(
        running_sha256,
        running_status,
        host.extension_service_worker_sha256_error.as_deref(),
        profile_install_state
            .active_profile_service_worker_sha256
            .as_deref(),
        profile_install_state
            .active_profile_extension_path
            .as_ref()
            .map(|path| {
                path.join("service_worker.js")
                    .to_string_lossy()
                    .into_owned()
            })
            .as_deref(),
        profile_install_state
            .active_profile_service_worker_error
            .as_deref(),
    ) {
        identity_reasons.push(reason);
    }
    if !identity_reasons.is_empty() {
        return Some(identity_reasons.join("|"));
    }
    if host.extension_debugger_api_available != Some(false) {
        return Some(format!(
            "debugger_api_available={} expected=false",
            host.extension_debugger_api_available
                .map_or_else(|| "not_seen_yet".to_owned(), |value| value.to_string())
        ));
    }
    if host.extension_capabilities.is_empty() {
        return Some(format!(
            "capabilities_not_advertised command={kind} required={}",
            REQUIRED_DIRECT_HTTP_CAPABILITIES.join(",")
        ));
    }
    if !host.extension_capabilities.contains(kind) {
        return Some(format!(
            "missing_capability={kind} loaded_capabilities={}",
            format_capabilities(&host.extension_capabilities)
        ));
    }
    None
}

fn service_worker_integrity_stale_reason(
    actual_sha256: Option<&str>,
    actual_status: Option<&str>,
    actual_error: Option<&str>,
    expected_sha256: Option<&str>,
    expected_worker_path: Option<&str>,
    expected_error: Option<&str>,
) -> Option<String> {
    let expected_worker_path =
        expected_worker_path.map_or_else(|| "<none>".to_owned(), quote_detail_value);
    let actual_status = actual_status.unwrap_or("not_seen_yet");
    let actual_error = actual_error.unwrap_or("none");
    match (actual_sha256, expected_sha256) {
        (Some(actual), Some(expected))
            if actual.trim().eq_ignore_ascii_case(expected.trim()) && actual_status == "ok" =>
        {
            None
        }
        (Some(actual), Some(expected)) => Some(format!(
            "service_worker_sha256={} expected={} service_worker_sha256_status={} service_worker_sha256_error={} expected_service_worker_path={}",
            actual.trim().to_ascii_lowercase(),
            expected.trim().to_ascii_lowercase(),
            actual_status,
            quote_detail_value(actual_error),
            expected_worker_path
        )),
        (None, Some(expected)) => Some(format!(
            "service_worker_sha256=not_seen_yet expected={} service_worker_sha256_status={} service_worker_sha256_error={} expected_service_worker_path={}",
            expected.trim().to_ascii_lowercase(),
            actual_status,
            quote_detail_value(actual_error),
            expected_worker_path
        )),
        (_, None) => Some(format!(
            "service_worker_sha256_expected=not_available service_worker_sha256_status={} service_worker_sha256_error={} expected_service_worker_path={} expected_profile_error={}",
            actual_status,
            quote_detail_value(actual_error),
            expected_worker_path,
            quote_detail_value(expected_error.unwrap_or("expected_service_worker_sha256_missing"))
        )),
    }
}

fn bridge_build_id_stale_reason(
    actual_build_id: Option<&str>,
    expected_build_id: &str,
    actual_service_worker_sha256: Option<&str>,
    expected_service_worker_sha256: Option<&str>,
    actual_service_worker_status: Option<&str>,
) -> Option<String> {
    if actual_build_id == Some(expected_build_id) {
        return None;
    }
    let actual_build_id = actual_build_id.unwrap_or("not_seen_yet");
    let worker_sha256_matches = actual_service_worker_sha256
        .zip(expected_service_worker_sha256)
        .is_some_and(|(actual, expected)| actual.trim().eq_ignore_ascii_case(expected.trim()))
        && actual_service_worker_status.unwrap_or("not_seen_yet") == "ok";
    if worker_sha256_matches {
        return Some(format!(
            "build_id_skew=daemon_expected_build_mismatch loaded_build_id={actual_build_id} expected_build_id={expected_build_id} service_worker_sha256_matches_expected=true repair=restart_or_reinstall_repo_built_daemon"
        ));
    }
    Some(format!(
        "build_id={actual_build_id} expected={expected_build_id}"
    ))
}

fn setup_repair_guidance() -> String {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_dir = manifest_dir.parent().and_then(Path::parent);
    if let Some(source_dir) = source_dir {
        let setup_script = source_dir.join("scripts").join("synapse-setup.ps1");
        if setup_script.is_file() {
            return format!(
                "{SETUP_REPAIR_MCP_GUIDANCE}; setup_script_path={} source_dir={} powershell_command={}",
                quote_detail_value(&setup_script.to_string_lossy()),
                quote_detail_value(&source_dir.to_string_lossy()),
                quote_detail_value(&format!(
                    "pwsh -NoProfile -ExecutionPolicy Bypass -File \"{}\" -SourceDir \"{}\" -ForceRestart",
                    setup_script.display(),
                    source_dir.display()
                ))
            );
        }
        return format!(
            "{SETUP_REPAIR_MCP_GUIDANCE}; setup_script_missing_at={} source_dir={}",
            quote_detail_value(&setup_script.to_string_lossy()),
            quote_detail_value(&source_dir.to_string_lossy())
        );
    }
    format!(
        "{SETUP_REPAIR_MCP_GUIDANCE}; source_dir_unresolved_from_cargo_manifest_dir={}",
        quote_detail_value(env!("CARGO_MANIFEST_DIR"))
    )
}

fn bridge_identity_stale_reasons(host: &ChromeBridgeHealthRecord) -> Vec<String> {
    let mut reasons = Vec::new();
    if host.extension_id.as_deref() != Some(EXTENSION_ID) {
        reasons.push(format!(
            "extension_id={} expected={EXTENSION_ID}",
            host.extension_id.as_deref().unwrap_or("not_seen_yet")
        ));
    }
    if let Some(reason) = bridge_build_id_stale_reason(
        host.extension_build_id.as_deref(),
        EXPECTED_EXTENSION_BUILD_ID,
        host.extension_service_worker_sha256.as_deref(),
        host.expected_service_worker_sha256.as_deref(),
        host.extension_service_worker_sha256_status.as_deref(),
    ) {
        reasons.push(reason);
    }
    if let Some(reason) = service_worker_integrity_stale_reason(
        host.extension_service_worker_sha256.as_deref(),
        host.extension_service_worker_sha256_status.as_deref(),
        host.extension_service_worker_sha256_error.as_deref(),
        host.expected_service_worker_sha256.as_deref(),
        host.expected_service_worker_path.as_deref(),
        None,
    ) {
        reasons.push(reason);
    }
    if host.extension_debugger_api_available != Some(false) {
        reasons.push(format!(
            "debugger_api_available={} expected=false",
            host.extension_debugger_api_available
                .map_or_else(|| "not_seen_yet".to_owned(), |value| value.to_string())
        ));
    }
    if let Some(reason) =
        reconnect_wake_alarm_stale_reason(host.extension_startup_readback.as_ref())
    {
        reasons.push(reason);
    }
    if let Some(reason) =
        durable_owner_startup_stale_reason(host.extension_startup_readback.as_ref())
    {
        reasons.push(reason);
    }
    let missing = bridge_missing_required_capabilities(&host.extension_capabilities);
    if !missing.is_empty() {
        reasons.push(format!("missing_capabilities={}", missing.join(",")));
    }
    reasons
}

fn durable_owner_startup_stale_reason(startup_readback: Option<&Value>) -> Option<String> {
    let Some(owner) = startup_readback.and_then(|value| value.get("durable_owner_state")) else {
        return Some(
            "durable_owner_state=not_seen_yet expected=loaded_continuous_mutation_enabled"
                .to_owned(),
        );
    };
    let loaded = owner
        .get("storage_state_loaded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let enabled = owner
        .get("mutation_admission_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let current_present = owner
        .get("browser_session_id_present")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ledger_present = owner
        .get("ledger_browser_session_id_present")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ids_match = owner
        .get("browser_session_ids_match")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let continuity = owner
        .get("browser_session_continuity_matched")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let healthy = owner
        .get("owner_continuity_healthy")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let stale_owners = owner
        .get("stale_browser_session_owner_count")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    let in_flight = owner
        .get("persisted_in_flight_mutation_present")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let unresolved_timeouts = owner
        .get("unresolved_debugger_command_timeout_count")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    let unresolved_restart = owner
        .get("unresolved_worker_restart_mutation_count")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    let capture_continuity = owner
        .get("capture_visible_tab_continuity_healthy")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let capture_lease = owner.get("capture_visible_tab_lease");
    let capture_lease_status = capture_lease
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("none");
    if !capture_continuity && capture_lease_status == "quarantined_worker_restart" {
        return Some(format!(
            "capture_visible_tab_lease_quarantined status={capture_lease_status} lease_id={} worker_boot_id={} tab_id={} window_id={} caller_timed_out_at_unix_ms={} remediation=fully_restart_Chrome_to_establish_a_new_browser_session; extension_reload_cannot_prove_raw_promise_settlement",
            quote_detail_value(
                capture_lease
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("missing")
            ),
            quote_detail_value(
                capture_lease
                    .and_then(|value| value.get("workerBootId"))
                    .and_then(Value::as_str)
                    .unwrap_or("missing")
            ),
            capture_lease
                .and_then(|value| value.get("tabId"))
                .and_then(Value::as_i64)
                .map_or_else(|| "missing".to_owned(), |value| value.to_string()),
            capture_lease
                .and_then(|value| value.get("windowId"))
                .and_then(Value::as_i64)
                .map_or_else(|| "missing".to_owned(), |value| value.to_string()),
            capture_lease
                .and_then(|value| value.get("callerTimedOutAtUnixMs"))
                .and_then(Value::as_i64)
                .map_or_else(|| "none".to_owned(), |value| value.to_string()),
        ));
    }
    let load_error = owner
        .get("storage_state_load_error")
        .and_then(Value::as_str)
        .unwrap_or("none");
    if loaded
        && enabled
        && current_present
        && ledger_present
        && ids_match
        && continuity
        && healthy
        && stale_owners == 0
        && !in_flight
        && unresolved_timeouts == 0
        && unresolved_restart == 0
        && capture_continuity
        && owner
            .get("storage_state_load_error")
            .is_some_and(Value::is_null)
    {
        return None;
    }
    Some(format!(
        "durable_owner_state_unhealthy loaded={loaded} enabled={enabled} current_session_present={current_present} ledger_session_present={ledger_present} session_ids_match={ids_match} continuity={continuity} healthy={healthy} stale_owners={stale_owners} in_flight={in_flight} unresolved_timeouts={unresolved_timeouts} unresolved_worker_restart={unresolved_restart} capture_visible_tab_continuity={capture_continuity} load_error={}",
        quote_detail_value(load_error),
    ))
}

fn reconnect_wake_alarm_stale_reason(startup_readback: Option<&Value>) -> Option<String> {
    let Some(readback) = startup_readback else {
        return Some(
            "startup_reconnect_alarm=not_seen_yet expected=persisted_read_verified".to_owned(),
        );
    };
    let Some(alarm) = readback.get("reconnect_alarm") else {
        return Some("startup_reconnect_alarm=missing expected=persisted_read_verified".to_owned());
    };
    let status = alarm
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("missing");
    let name = alarm
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("missing");
    let period_minutes = alarm
        .get("period_minutes")
        .and_then(Value::as_f64)
        .unwrap_or(f64::NAN);
    let persisted = alarm
        .get("persist_across_sessions")
        .and_then(Value::as_bool);
    let checked_at_unix_ms = alarm
        .get("checked_at_unix_ms")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let scheduled_time = alarm
        .get("scheduled_time")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let period_matches =
        (period_minutes - RECONNECT_WAKE_ALARM_PERIOD_MINUTES).abs() < f64::EPSILON;
    let schedule_was_future_at_readback =
        checked_at_unix_ms > 0.0 && scheduled_time > checked_at_unix_ms;
    if status == "ok"
        && name == RECONNECT_WAKE_ALARM_NAME
        && period_matches
        && persisted == Some(true)
        && schedule_was_future_at_readback
    {
        return None;
    }
    Some(format!(
        "startup_reconnect_alarm_status={status} name={name} period_minutes={period_minutes} persist_across_sessions={} checked_at_unix_ms={checked_at_unix_ms} scheduled_time={scheduled_time} expected_name={RECONNECT_WAKE_ALARM_NAME} expected_period_minutes={RECONNECT_WAKE_ALARM_PERIOD_MINUTES} expected_persist_across_sessions=true expected_future_schedule_at_readback=true",
        persisted.map_or_else(|| "missing".to_owned(), |value| value.to_string())
    ))
}

fn host_record_to_health_record(host_id: &str, host: &HostRecord) -> ChromeBridgeHealthRecord {
    let profile_install_state = synapse_chrome_profile_install_state();
    host_record_to_health_record_with_profile_state(host_id, host, &profile_install_state)
}

fn host_record_to_health_record_with_profile_state(
    host_id: &str,
    host: &HostRecord,
    profile_install_state: &SynapseChromeProfileInstallState,
) -> ChromeBridgeHealthRecord {
    ChromeBridgeHealthRecord {
        host_id: host_id.to_owned(),
        origin: host.origin.clone(),
        extension_id: host.extension_id.clone(),
        extension_version: host.extension_version.clone(),
        extension_protocol_version: host.extension_protocol_version,
        extension_build_id: host.extension_build_id.clone(),
        extension_build_sha256: host.extension_build_sha256.clone(),
        extension_declared_build_sha256: host.extension_declared_build_sha256.clone(),
        extension_service_worker_sha256: host.extension_service_worker_sha256.clone(),
        extension_service_worker_sha256_status: host.extension_service_worker_sha256_status.clone(),
        extension_service_worker_sha256_source: host.extension_service_worker_sha256_source.clone(),
        extension_service_worker_byte_length: host.extension_service_worker_byte_length,
        extension_service_worker_sha256_error: host.extension_service_worker_sha256_error.clone(),
        expected_service_worker_sha256: profile_install_state
            .active_profile_service_worker_sha256
            .clone(),
        expected_service_worker_path: profile_install_state
            .active_profile_extension_path
            .as_ref()
            .map(|path| {
                path.join("service_worker.js")
                    .to_string_lossy()
                    .into_owned()
            }),
        extension_capabilities: host.extension_capabilities.clone(),
        extension_user_agent: host.extension_user_agent.clone(),
        extension_debugger_api_available: host.extension_debugger_api_available,
        extension_popup_risk_suppression: host.extension_popup_risk_suppression.clone(),
        extension_startup_readback: host.extension_startup_readback.clone(),
        pid: host.pid,
        parent_window: host.parent_window.clone(),
        transport: host.transport.clone(),
        registered_unix_ms: host.registered_unix_ms,
        last_seen_unix_ms: host.last_seen_unix_ms,
        last_disconnect_detail: host.last_disconnect_detail.clone(),
        last_detach_reason: host.last_detach_reason.clone(),
    }
}

fn health_record_to_host_snapshot(host: &ChromeBridgeHealthRecord) -> ChromeBridgeHostSnapshot {
    let stale_reasons = bridge_identity_stale_reasons(host);
    ChromeBridgeHostSnapshot {
        host_id: host.host_id.clone(),
        origin: host.origin.clone(),
        extension_id: host.extension_id.clone(),
        extension_version: host.extension_version.clone(),
        extension_protocol_version: host.extension_protocol_version,
        extension_build_id: host.extension_build_id.clone(),
        extension_build_sha256: host.extension_build_sha256.clone(),
        extension_declared_build_sha256: host.extension_declared_build_sha256.clone(),
        extension_service_worker_sha256: host.extension_service_worker_sha256.clone(),
        extension_service_worker_sha256_status: host.extension_service_worker_sha256_status.clone(),
        extension_service_worker_sha256_source: host.extension_service_worker_sha256_source.clone(),
        extension_service_worker_byte_length: host.extension_service_worker_byte_length,
        extension_service_worker_sha256_error: host.extension_service_worker_sha256_error.clone(),
        expected_service_worker_sha256: host.expected_service_worker_sha256.clone(),
        expected_service_worker_path: host.expected_service_worker_path.clone(),
        extension_capabilities: host.extension_capabilities.iter().cloned().collect(),
        extension_user_agent: host.extension_user_agent.clone(),
        extension_debugger_api_available: host.extension_debugger_api_available,
        extension_popup_risk_suppression: host.extension_popup_risk_suppression.clone(),
        extension_startup_readback: host.extension_startup_readback.clone(),
        pid: host.pid,
        parent_window: host.parent_window.clone(),
        transport: host.transport.clone(),
        registered_unix_ms: host.registered_unix_ms,
        last_seen_unix_ms: host.last_seen_unix_ms,
        last_disconnect_detail: host.last_disconnect_detail.clone(),
        last_detach_reason: host.last_detach_reason.clone(),
        extension_stale: !stale_reasons.is_empty(),
        extension_stale_reasons: stale_reasons,
    }
}

fn json_pointer_required_str<'a>(
    value: &'a Value,
    pointer: &str,
) -> Result<&'a str, ChromeDebuggerBridgeError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ChromeDebuggerBridgeError::host_reload_failed(format!(
                "SYNAPSE_CHROME_BRIDGE_HOST_RELOAD_READBACK_INVALID field={pointer} expected=non_empty_string remediation=inspect the framed installer JSON and repair the named missing or mistyped field"
            ))
        })
}

fn json_pointer_required_bool(
    value: &Value,
    pointer: &str,
) -> Result<bool, ChromeDebuggerBridgeError> {
    value
        .pointer(pointer)
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            ChromeDebuggerBridgeError::host_reload_failed(format!(
                "SYNAPSE_CHROME_BRIDGE_HOST_RELOAD_READBACK_INVALID field={pointer} expected=boolean remediation=inspect the framed installer JSON and repair the named missing or mistyped field"
            ))
        })
}

fn process_diagnostic(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let chars = text.chars().count();
    let bounded = if chars > 2_000 {
        text.chars().skip(chars - 2_000).collect::<String>()
    } else {
        text.into_owned()
    };
    bounded.replace(['\r', '\n'], " | ")
}

#[cfg(windows)]
async fn run_chrome_bridge_background_deploy()
-> Result<ChromeBridgeBackgroundDeployReadback, ChromeDebuggerBridgeError> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_dir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| {
            ChromeDebuggerBridgeError::host_reload_failed(format!(
                "SYNAPSE_CHROME_BACKGROUND_DEPLOY_SOURCE_DIR_INVALID cargo_manifest_dir={} remediation=rebuild synapse-mcp from a real Synapse checkout containing scripts\\install-synapse-chrome-debugger.ps1",
                manifest_dir.display()
            ))
        })?;
    let installer_path = source_dir
        .join("scripts")
        .join("install-synapse-chrome-debugger.ps1");
    let installer_bytes = std::fs::read(&installer_path).map_err(|error| {
        ChromeDebuggerBridgeError::host_reload_failed(format!(
            "SYNAPSE_CHROME_BACKGROUND_DEPLOY_INSTALLER_READ_FAILED path={} error={} remediation=restore the repo installer at the compiled source checkout and retry browser_debugger reload_bridge",
            installer_path.display(),
            error
        ))
    })?;
    let installer_sha256 = sha256_hex_lower(&installer_bytes);
    let system_root = std::env::var_os("SystemRoot").ok_or_else(|| {
        ChromeDebuggerBridgeError::host_reload_failed(
            "SYNAPSE_CHROME_BACKGROUND_DEPLOY_SYSTEM_ROOT_MISSING remediation=repair the Windows process environment so SystemRoot resolves Windows PowerShell",
        )
    })?;
    let powershell_path = PathBuf::from(system_root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !powershell_path.is_file() {
        return Err(ChromeDebuggerBridgeError::host_reload_failed(format!(
            "SYNAPSE_CHROME_BACKGROUND_DEPLOY_POWERSHELL_MISSING path={} remediation=repair the configured Windows PowerShell installation before retrying the background-only deploy",
            powershell_path.display()
        )));
    }

    let mut command = Command::new(&powershell_path);
    command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&installer_path)
        .args(["-SkipAutoInstall", "-OutputJson"])
        .current_dir(source_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().creation_flags(CREATE_NO_WINDOW);

    let started = Instant::now();
    let child = command.spawn().map_err(|error| {
        ChromeDebuggerBridgeError::host_reload_failed(format!(
            "SYNAPSE_CHROME_BACKGROUND_DEPLOY_PROCESS_SPAWN_FAILED launcher={} installer={} error={} remediation=repair PowerShell or source-checkout process permissions and retry browser_debugger reload_bridge",
            powershell_path.display(),
            installer_path.display(),
            error
        ))
    })?;
    let child_pid = child.id().unwrap_or(0);
    let output = timeout(BACKGROUND_DEPLOY_PROCESS_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            ChromeDebuggerBridgeError::host_reload_failed(format!(
                "SYNAPSE_CHROME_BACKGROUND_DEPLOY_PROCESS_TIMEOUT pid={} timeout_ms={} installer={} installer_sha256={} remediation=the hidden background deploy process was terminated; inspect the installer output and physical extension directory without interacting with a human Chrome window",
                child_pid,
                BACKGROUND_DEPLOY_PROCESS_TIMEOUT.as_millis(),
                installer_path.display(),
                installer_sha256
            ))
        })?
        .map_err(|error| {
            ChromeDebuggerBridgeError::host_reload_failed(format!(
                "SYNAPSE_CHROME_BACKGROUND_DEPLOY_PROCESS_WAIT_FAILED pid={} installer={} error={} remediation=inspect Windows process state and retry the background-only deploy",
                child_pid,
                installer_path.display(),
                error
            ))
        })?;
    let installer_duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let installer_exit_code = output.status.code().unwrap_or(-1);
    let installer_stdout_sha256 = sha256_hex_lower(&output.stdout);
    let installer_stderr_sha256 = sha256_hex_lower(&output.stderr);
    if !output.status.success() {
        return Err(ChromeDebuggerBridgeError::host_reload_failed(format!(
            "SYNAPSE_CHROME_BACKGROUND_DEPLOY_PROCESS_FAILED pid={} exit_code={} installer={} installer_sha256={} stdout_sha256={} stderr_sha256={} stdout_tail={} stderr_tail={} remediation=repair the exact SYNAPSE_CHROME_* condition named by the installer; the deploy failed before any foreground API, tab mutation, or synthetic input",
            child_pid,
            installer_exit_code,
            installer_path.display(),
            installer_sha256,
            installer_stdout_sha256,
            installer_stderr_sha256,
            process_diagnostic(&output.stdout),
            process_diagnostic(&output.stderr)
        )));
    }
    let stdout = String::from_utf8(output.stdout).map_err(|error| {
        ChromeDebuggerBridgeError::host_reload_failed(format!(
            "SYNAPSE_CHROME_BACKGROUND_DEPLOY_STDOUT_UTF8_INVALID pid={child_pid} stdout_sha256={installer_stdout_sha256} error={error} remediation=repair the installer process output encoding and retry"
        ))
    })?;
    let payload_rows = stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix(BACKGROUND_DEPLOY_JSON_PREFIX))
        .collect::<Vec<_>>();
    if payload_rows.len() != 1 {
        return Err(ChromeDebuggerBridgeError::host_reload_failed(format!(
            "SYNAPSE_CHROME_BACKGROUND_DEPLOY_JSON_FRAME_INVALID pid={} frame_count={} stdout_sha256={} stderr_sha256={} stdout_tail={} remediation=the installer must emit exactly one {} frame after background deployment",
            child_pid,
            payload_rows.len(),
            installer_stdout_sha256,
            installer_stderr_sha256,
            process_diagnostic(stdout.as_bytes()),
            BACKGROUND_DEPLOY_JSON_PREFIX
        )));
    }
    let payload = BASE64_STANDARD.decode(payload_rows[0]).map_err(|error| {
        ChromeDebuggerBridgeError::host_reload_failed(format!(
            "SYNAPSE_CHROME_BACKGROUND_DEPLOY_JSON_BASE64_INVALID pid={child_pid} stdout_sha256={installer_stdout_sha256} error={error} remediation=repair the installer JSON frame encoder and retry"
        ))
    })?;
    let readback: Value = serde_json::from_slice(&payload).map_err(|error| {
        ChromeDebuggerBridgeError::host_reload_failed(format!(
            "SYNAPSE_CHROME_BACKGROUND_DEPLOY_JSON_INVALID pid={} payload_sha256={} error={} remediation=repair the installer JSON serializer and retry",
            child_pid,
            sha256_hex_lower(&payload),
            error
        ))
    })?;

    let ok = json_pointer_required_bool(&readback, "/ok")?;
    let extension_id = json_pointer_required_str(&readback, "/extension_id")?;
    let extension_dir = json_pointer_required_str(&readback, "/extension_dir")?.to_owned();
    let deployed_build_id = json_pointer_required_str(&readback, "/extension_deploy/build_id")?;
    let declared_build_sha256 =
        json_pointer_required_str(&readback, "/extension_deploy/declared_build_sha256")?;
    let extension_service_worker_sha256 =
        json_pointer_required_str(&readback, "/extension_deploy/service_worker_sha256")?.to_owned();
    let byte_exact = json_pointer_required_bool(
        &readback,
        "/extension_deploy/service_worker_byte_exact_readback",
    )?;
    let register_token_matches = json_pointer_required_bool(
        &readback,
        "/extension_deploy/bridge_register_token_matches_expected",
    )?;
    let profile_scanned =
        json_pointer_required_bool(&readback, "/synapse_chrome_profile_install_state/scanned")?;
    let profile_installed =
        json_pointer_required_bool(&readback, "/synapse_chrome_profile_install_state/installed")?;
    let active_profile_installed = json_pointer_required_bool(
        &readback,
        "/synapse_chrome_profile_install_state/active_profile_installed",
    )?;
    let active_profile =
        json_pointer_required_str(&readback, "/synapse_chrome_auto_install/active_profile")?
            .to_owned();
    let attempted =
        json_pointer_required_bool(&readback, "/synapse_chrome_auto_install/attempted")?;
    let changed = json_pointer_required_bool(&readback, "/synapse_chrome_auto_install/changed")?;
    let reason = json_pointer_required_str(&readback, "/synapse_chrome_auto_install/reason")?;
    let required_foreground = json_pointer_required_bool(
        &readback,
        "/synapse_chrome_auto_install/required_foreground",
    )?;
    let profile_before_installed =
        json_pointer_required_bool(&readback, "/synapse_chrome_auto_install/before/installed")?;
    let profile_before_ready =
        json_pointer_required_bool(&readback, "/synapse_chrome_auto_install/before/ready")?;
    let profile_after_installed =
        json_pointer_required_bool(&readback, "/synapse_chrome_auto_install/after/installed")?;
    let profile_after_ready =
        json_pointer_required_bool(&readback, "/synapse_chrome_auto_install/after/ready")?;
    if !ok
        || extension_id != EXTENSION_ID
        || deployed_build_id != EXPECTED_EXTENSION_BUILD_ID
        || declared_build_sha256 != EXPECTED_EXTENSION_DECLARED_BUILD_SHA256
        || extension_service_worker_sha256.len() != 64
        || !byte_exact
        || !register_token_matches
        || !profile_scanned
        || !profile_installed
        || !active_profile_installed
        || attempted
        || changed
        || reason != "skip_auto_install_requested"
        || required_foreground
        || !profile_before_installed
        || !profile_before_ready
        || !profile_after_installed
        || !profile_after_ready
    {
        return Err(ChromeDebuggerBridgeError::host_reload_failed(format!(
            "SYNAPSE_CHROME_BACKGROUND_DEPLOY_POSTCONDITION_FAILED ok={} extension_id={} expected_extension_id={} deployed_build_id={} expected_build_id={} declared_build_sha256={} expected_declared_build_sha256={} service_worker_sha256={} byte_exact={} register_token_matches={} profile_scanned={} profile_installed={} active_profile={} active_profile_installed={} attempted={} changed={} reason={} required_foreground={} before_installed={} before_ready={} after_installed={} after_ready={} installer={} installer_sha256={} remediation=repair the exact deployed-file or physical Chrome profile-row mismatch; Synapse refuses to reload partial, unready, or foreground-dependent state",
            ok,
            extension_id,
            EXTENSION_ID,
            deployed_build_id,
            EXPECTED_EXTENSION_BUILD_ID,
            declared_build_sha256,
            EXPECTED_EXTENSION_DECLARED_BUILD_SHA256,
            extension_service_worker_sha256,
            byte_exact,
            register_token_matches,
            profile_scanned,
            profile_installed,
            active_profile,
            active_profile_installed,
            attempted,
            changed,
            reason,
            required_foreground,
            profile_before_installed,
            profile_before_ready,
            profile_after_installed,
            profile_after_ready,
            installer_path.display(),
            installer_sha256
        )));
    }
    Ok(ChromeBridgeBackgroundDeployReadback {
        installer_path: installer_path.display().to_string(),
        installer_sha256,
        installer_exit_code,
        installer_stdout_sha256,
        installer_stderr_sha256,
        installer_duration_ms,
        extension_dir,
        extension_service_worker_sha256,
        active_profile,
        profile_before_installed,
        profile_before_ready,
        profile_after_installed,
        profile_after_ready,
    })
}

#[cfg(not(windows))]
async fn run_chrome_bridge_background_deploy()
-> Result<ChromeBridgeBackgroundDeployReadback, ChromeDebuggerBridgeError> {
    Err(ChromeDebuggerBridgeError::host_reload_failed(
        "SYNAPSE_CHROME_BACKGROUND_DEPLOY_UNSUPPORTED_OS os=non_windows remediation=run the normal-profile Chrome bridge lifecycle on its configured Windows host",
    ))
}

impl ChromeDebuggerBridge {
    fn register(&self, request: NativeRegisterRequest) -> Result<NativeRegisterResponse, String> {
        if request.bridge_protocol_version != BRIDGE_PROTOCOL_VERSION {
            return Err(format!(
                "bridge protocol mismatch: host={} daemon={}",
                request.bridge_protocol_version, BRIDGE_PROTOCOL_VERSION
            ));
        }
        if !request.origin.starts_with("chrome-extension://") || !request.origin.ends_with('/') {
            return Err(format!(
                "native host origin must be a chrome-extension:// origin with trailing slash, got {:?}",
                request.origin
            ));
        }
        let now = now_unix_ms();
        let host_id = format!("chrome-native-{}-{}", request.pid, now);
        let bridge_token = Uuid::new_v4().to_string();
        let record = HostRecord {
            origin: request.origin,
            extension_id: None,
            extension_version: None,
            extension_protocol_version: None,
            extension_build_id: None,
            extension_build_sha256: None,
            extension_declared_build_sha256: None,
            extension_service_worker_sha256: None,
            extension_service_worker_sha256_status: None,
            extension_service_worker_sha256_source: None,
            extension_service_worker_byte_length: None,
            extension_service_worker_sha256_error: None,
            extension_running_service_worker_sha256: None,
            extension_running_service_worker_sha256_status: None,
            extension_capabilities: BTreeSet::new(),
            extension_user_agent: None,
            extension_debugger_api_available: None,
            extension_popup_risk_suppression: None,
            extension_startup_readback: None,
            pid: request.pid,
            parent_window: request.parent_window,
            transport: request.transport,
            bridge_token_digest: digest_bridge_token(&bridge_token),
            registered_unix_ms: now,
            last_seen_unix_ms: now,
            last_disconnect_detail: None,
            last_detach_reason: None,
        };
        let transport_label = record
            .transport
            .as_deref()
            .unwrap_or("native_messaging")
            .to_owned();
        let is_direct_http = transport_label == "direct_http";
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "chrome debugger bridge lock poisoned during register".to_owned())?;
        if is_direct_http {
            replace_active_direct_http_host(&mut inner, &host_id);
        }
        inner.host_lineage_sequence = inner.host_lineage_sequence.saturating_add(1);
        inner.active_host_id = Some(host_id.clone());
        inner.hosts.insert(host_id.clone(), record);
        tracing::info!(
            code = "CHROME_DEBUGGER_NATIVE_HOST_REGISTERED",
            host_id = %host_id,
            pid = request.pid,
            transport = %transport_label,
            "Chrome debugger native host registered"
        );
        Ok(NativeRegisterResponse {
            ok: true,
            host_id,
            bridge_token,
            bridge_protocol_version: BRIDGE_PROTOCOL_VERSION,
            native_host_name: NATIVE_HOST_NAME.to_owned(),
            expected_extension_id: EXTENSION_ID.to_owned(),
        })
    }

    fn post_message(&self, request: NativeMessageRequest) -> Result<(), String> {
        let mut browser_navigation_event = None;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "chrome debugger bridge lock poisoned during message".to_owned())?;
        let Some(host) = inner.hosts.get_mut(&request.host_id) else {
            return Err(format!(
                "unknown chrome debugger native host_id {:?}",
                request.host_id
            ));
        };
        host.last_seen_unix_ms = now_unix_ms();
        let message_type = request
            .message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match message_type {
            "hello" => {
                // #1880: a re-registration that changes nothing is not evidence.
                // Snapshot the identity this host is already registered under so
                // the log can distinguish a real identity change (INFO) from a
                // gratuitous repeat (DEBUG). At the 30 s reconnect cadence the
                // repeats were 3.3% of every line the daemon wrote and would
                // have hidden a genuine bridge fault completely.
                let previous_identity = ChromeBridgeHelloIdentity::of(host);
                host.extension_id = request
                    .message
                    .get("extensionId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                host.extension_version = request
                    .message
                    .get("version")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                host.extension_protocol_version = request
                    .message
                    .get("protocolVersion")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok());
                host.extension_build_id = request
                    .message
                    .get("buildId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                host.extension_build_sha256 = request
                    .message
                    .get("buildSha256")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                host.extension_declared_build_sha256 = request
                    .message
                    .get("declaredBuildSha256")
                    .and_then(Value::as_str)
                    .or_else(|| request.message.get("buildSha256").and_then(Value::as_str))
                    .map(ToOwned::to_owned);
                host.extension_service_worker_sha256 = request
                    .message
                    .get("serviceWorkerSha256")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                host.extension_service_worker_sha256_status = request
                    .message
                    .get("serviceWorkerSha256Status")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                host.extension_service_worker_sha256_source = request
                    .message
                    .get("serviceWorkerSha256Source")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                host.extension_service_worker_byte_length = request
                    .message
                    .get("serviceWorkerByteLength")
                    .and_then(Value::as_u64);
                host.extension_service_worker_sha256_error = request
                    .message
                    .get("serviceWorkerSha256Error")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                host.extension_running_service_worker_sha256 = request
                    .message
                    .get("runningServiceWorkerSha256")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                host.extension_running_service_worker_sha256_status = request
                    .message
                    .get("runningServiceWorkerSha256Status")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                host.extension_user_agent = request
                    .message
                    .get("userAgent")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                host.extension_debugger_api_available = request
                    .message
                    .get("debuggerApiAvailable")
                    .and_then(Value::as_bool);
                host.extension_capabilities = string_set_field(&request.message, "capabilities");
                host.extension_popup_risk_suppression =
                    request.message.get("popupRiskSuppression").cloned();
                host.extension_startup_readback = request.message.get("startupReadback").cloned();
                let popup_risk_suppression =
                    popup_risk_suppression_summary(&host.extension_popup_risk_suppression);
                let startup_readback = host
                    .extension_startup_readback
                    .as_ref()
                    .map_or_else(|| "not_seen_yet".to_owned(), Value::to_string);
                let identity_changed = previous_identity != ChromeBridgeHelloIdentity::of(host);
                if !identity_changed {
                    // A repeat that reproduces the registered identity exactly.
                    // Recorded so the cadence stays observable, but without the
                    // full field dump and without spending the INFO budget on a
                    // non-event (#1880).
                    tracing::debug!(
                        code = "CHROME_DEBUGGER_EXTENSION_HELLO_UNCHANGED",
                        host_id = %request.host_id,
                        origin = %host.origin,
                        extension_id = host.extension_id.as_deref().unwrap_or_default(),
                        last_seen_unix_ms = host.last_seen_unix_ms,
                        registered_unix_ms = host.registered_unix_ms,
                        "Chrome debugger extension re-registered with an identical identity"
                    );
                    return Ok(());
                }
                tracing::info!(
                    code = "CHROME_DEBUGGER_EXTENSION_HELLO",
                    identity_changed,
                    host_id = %request.host_id,
                    origin = %host.origin,
                    extension_id = host.extension_id.as_deref().unwrap_or_default(),
                    extension_version = host.extension_version.as_deref().unwrap_or_default(),
                    extension_protocol_version = host.extension_protocol_version.unwrap_or_default(),
                    extension_build_id = host.extension_build_id.as_deref().unwrap_or_default(),
                    extension_declared_build_sha256 = host.extension_declared_build_sha256.as_deref().unwrap_or_default(),
                    extension_service_worker_sha256 = host.extension_service_worker_sha256.as_deref().unwrap_or_default(),
                    extension_service_worker_sha256_status = host.extension_service_worker_sha256_status.as_deref().unwrap_or_default(),
                    extension_service_worker_sha256_source = host.extension_service_worker_sha256_source.as_deref().unwrap_or_default(),
                    extension_service_worker_byte_length = host.extension_service_worker_byte_length.unwrap_or_default(),
                    extension_service_worker_sha256_error = host.extension_service_worker_sha256_error.as_deref().unwrap_or_default(),
                    debugger_api_available = host.extension_debugger_api_available.unwrap_or(true),
                    capabilities = %format_capabilities(&host.extension_capabilities),
                    popup_risk_suppression = %popup_risk_suppression,
                    startup_readback = %startup_readback,
                    pid = host.pid,
                    parent_window = host.parent_window.as_deref().unwrap_or_default(),
                    transport = host.transport.as_deref().unwrap_or("native_messaging"),
                    registered_unix_ms = host.registered_unix_ms,
                    "Chrome debugger extension connected through native host"
                );
            }
            "response" => {
                return Err(format!(
                    "{}: HTTP command responses are retired in bridge protocol v{}; send a durable command_terminal envelope on the authenticated command WebSocket and wait for its matching acknowledgement",
                    error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                    BRIDGE_PROTOCOL_VERSION,
                ));
            }
            "event" => {
                let event = request
                    .message
                    .get("event")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if event == "debuggerDetached" {
                    host.last_detach_reason = request
                        .message
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    tracing::warn!(
                        code = error_codes::A11Y_CDP_EXTENSION_DETACHED,
                        host_id = %request.host_id,
                        reason = host.last_detach_reason.as_deref().unwrap_or_default(),
                        "Chrome debugger session detached"
                    );
                } else if event == "nativePortDisconnected" {
                    let detail = request
                        .message
                        .get("detail")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    host.last_disconnect_detail = detail.clone();
                    let detail_for_log = host.last_disconnect_detail.clone().unwrap_or_default();
                    let _ = host;
                    if inner.active_host_id.as_deref() == Some(request.host_id.as_str()) {
                        inner.active_host_id = None;
                        inner.host_lineage_sequence = inner.host_lineage_sequence.saturating_add(1);
                    }
                    let terminal_detail = detail.unwrap_or_else(|| {
                        "Chrome debugger native port disconnected before command response"
                            .to_owned()
                    });
                    let stats = preserve_delivered_mutations_on_transport_loss(
                        &mut inner,
                        &request.host_id,
                        &terminal_detail,
                    );
                    tracing::warn!(
                        code = "CHROME_DEBUGGER_NATIVE_PORT_DISCONNECTED",
                        host_id = %request.host_id,
                        detail = %detail_for_log,
                        queued_removed = stats.queued_removed,
                        pending_failed_before_delivery = stats.pending_failed_before_delivery,
                        delivered_commands_preserved = stats.delivered_commands_preserved,
                        "Chrome debugger native port disconnected"
                    );
                } else if event == "tabNavigation" || event == "navigationClaim" {
                    let decoded = serde_json::from_value::<ExtensionTabNavigationEvent>(
                        request.message.clone(),
                    )
                    .map_err(|error| format!("decode Chrome tabNavigation event: {error}"))?;
                    browser_navigation_event = Some(ChromeDebuggerBrowserNavigationEvent {
                        event_id: decoded.event_id,
                        source: decoded.source,
                        event: event.to_owned(),
                        url: decoded.url,
                        title: decoded.title,
                        tab_id: Some(decoded.tab_id),
                        chrome_window_id: decoded.chrome_window_id,
                        cdp_target_id: (!decoded.target_id.is_empty()).then_some(decoded.target_id),
                        endpoint: host
                            .extension_id
                            .as_deref()
                            .map(|extension_id| format!("chrome-extension://{extension_id}")),
                        transport: host.transport.clone(),
                        ready_state: (!decoded.status.is_empty()).then_some(decoded.status),
                        observed_at_unix_ms: decoded.observed_at_unix_ms,
                        active: Some(decoded.active),
                        highlighted: Some(decoded.highlighted),
                        pinned: Some(decoded.pinned),
                        initiator: decoded.initiator,
                        claim_id: decoded.claim_id,
                        claim_status: decoded.claim_status,
                        agent_session_id: decoded.agent_session_id,
                        action: decoded.action,
                        requested_url: decoded.requested_url,
                        before_url: decoded.before_url,
                        before_document_id: decoded.before_document_id,
                        correlation_verdict: decoded.correlation_verdict,
                        frame_id: decoded.frame_id,
                        parent_frame_id: decoded.parent_frame_id,
                        document_id: decoded.document_id,
                        parent_document_id: decoded.parent_document_id,
                        document_lifecycle: decoded.document_lifecycle,
                        frame_type: decoded.frame_type,
                        transition_type: decoded.transition_type,
                        transition_qualifiers: decoded.transition_qualifiers,
                        navigation_error: decoded.navigation_error,
                        navigation_timestamp_ms: decoded.navigation_timestamp_ms,
                    });
                    tracing::info!(
                        code = "CHROME_DEBUGGER_BROWSER_NAVIGATION_EVENT",
                        host_id = %request.host_id,
                        event_id = %browser_navigation_event.as_ref().map_or("", |value| value.event_id.as_str()),
                        correlation_verdict = %browser_navigation_event.as_ref().map_or("", |value| value.correlation_verdict.as_str()),
                        "Chrome debugger tab navigation event accepted"
                    );
                } else {
                    tracing::info!(
                        code = "CHROME_DEBUGGER_EXTENSION_EVENT",
                        host_id = %request.host_id,
                        event = %event,
                        "Chrome debugger extension event"
                    );
                }
            }
            "log" => {
                tracing::info!(
                    code = "CHROME_DEBUGGER_EXTENSION_LOG",
                    host_id = %request.host_id,
                    message = request.message.get("message").and_then(|value| value.as_str()).unwrap_or_default(),
                    "Chrome debugger extension log"
                );
            }
            other => {
                tracing::warn!(
                    code = "CHROME_DEBUGGER_MESSAGE_UNKNOWN",
                    host_id = %request.host_id,
                    message_type = %other,
                    "unknown Chrome debugger extension message"
                );
            }
        }
        drop(inner);
        if let Some(event) = browser_navigation_event {
            emit_browser_navigation_event(event);
        }
        Ok(())
    }

    fn active_host_snapshot(&self) -> Result<ChromeBridgeHostSnapshot, ChromeDebuggerBridgeError> {
        let inner = self.inner.lock().map_err(|_| {
            ChromeDebuggerBridgeError::protocol(
                "chrome debugger bridge lock poisoned during host snapshot",
            )
        })?;
        let Some(host_id) = inner.active_host_id.as_deref() else {
            return Err(ChromeDebuggerBridgeError::unavailable());
        };
        let Some(host) = inner.hosts.get(host_id) else {
            return Err(ChromeDebuggerBridgeError::unavailable());
        };
        Ok(health_record_to_host_snapshot(
            &host_record_to_health_record(host_id, host),
        ))
    }

    async fn reload_in_background(
        &self,
        wait_timeout_ms: u64,
    ) -> Result<ChromeBridgeReloadResult, ChromeDebuggerBridgeError> {
        let wait_timeout = Duration::from_millis(wait_timeout_ms);
        invalidate_chrome_profile_scan_cache("background_reload_before_snapshot");
        let before = self.active_host_snapshot().map_err(|error| {
            ChromeDebuggerBridgeError::host_reload_failed(format!(
                "SYNAPSE_CHROME_BACKGROUND_RELOAD_HOST_UNAVAILABLE error_code={} detail_sha256={} {}",
                error.code(),
                sha256_hex_lower(error.detail().as_bytes()),
                NO_ACTIVE_HOST_REPAIR_GUIDANCE
            ))
        })?;
        let loaded_build_id = before
            .extension_build_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ChromeDebuggerBridgeError::host_reload_failed(format!(
                    "SYNAPSE_CHROME_BACKGROUND_RELOAD_LOADED_BUILD_MISSING host_id={} remediation=restore a bridge build that advertises its executable identity and reloadSelf capability; Synapse failed before deploying files or touching any human window",
                    before.host_id
                ))
            })?
            .to_owned();
        if before.extension_id.as_deref() != Some(EXTENSION_ID)
            || !before
                .extension_capabilities
                .iter()
                .any(|capability| capability == BACKGROUND_RUNTIME_RELOAD_COMMAND)
            || before.last_disconnect_detail.is_some()
        {
            return Err(ChromeDebuggerBridgeError::host_reload_failed(format!(
                "SYNAPSE_CHROME_BACKGROUND_RELOAD_HOST_INELIGIBLE host_id={} extension_id={} expected_extension_id={} loaded_build_id={} reload_self_capability={} last_disconnect_detail={} remediation=restore an authenticated connected bridge that advertises reloadSelf; Synapse failed before deploying files or touching any human window",
                before.host_id,
                before.extension_id.as_deref().unwrap_or("<missing>"),
                EXTENSION_ID,
                loaded_build_id,
                before
                    .extension_capabilities
                    .iter()
                    .any(|capability| capability == BACKGROUND_RUNTIME_RELOAD_COMMAND),
                before.last_disconnect_detail.as_deref().unwrap_or("<none>")
            )));
        }

        let deploy = run_chrome_bridge_background_deploy().await?;
        invalidate_chrome_profile_scan_cache("background_reload_after_deploy");
        let deployed_profile = synapse_chrome_profile_install_state();
        let profile_worker_sha256 = deployed_profile
            .active_profile_service_worker_sha256
            .as_deref()
            .unwrap_or("<missing>");
        let deployed_profile_installed = deployed_profile.active_profile_extension_path.is_some();
        if !deployed_profile_installed
            || profile_worker_sha256 != deploy.extension_service_worker_sha256
        {
            return Err(ChromeDebuggerBridgeError::host_reload_failed(format!(
                "SYNAPSE_CHROME_BACKGROUND_DEPLOY_INDEPENDENT_READBACK_FAILED active_profile={} active_profile_installed={} deployed_worker_sha256={} profile_worker_sha256={} profile_error={} remediation=repair the stable extension directory or physical Chrome profile row; Synapse refuses to signal reload when a separate daemon read does not match the deployed bytes",
                deploy.active_profile,
                deployed_profile_installed,
                deploy.extension_service_worker_sha256,
                profile_worker_sha256,
                deployed_profile
                    .active_profile_service_worker_error
                    .as_deref()
                    .unwrap_or("<none>")
            )));
        }

        let result = self
            .send_command_with_timeout(
                BACKGROUND_RUNTIME_RELOAD_COMMAND,
                json!({
                    "expectedExtensionId": EXTENSION_ID,
                    "expectedLoadedBuildId": loaded_build_id,
                    "expectedDeployedBuildId": EXPECTED_EXTENSION_BUILD_ID,
                    "reloadDelayMs": 500,
                }),
                Duration::from_secs(5),
            )
            .await?;
        let runtime_ack =
            serde_json::from_value::<ChromeBridgeRuntimeReloadAck>(result).map_err(|error| {
                ChromeDebuggerBridgeError::protocol(format!(
                    "decode Chrome bridge reloadSelf acknowledgement: {error}"
                ))
            })?;
        if !runtime_ack.ok
            || runtime_ack.control_surface != "chrome.runtime.reload"
            || runtime_ack.required_foreground
            || runtime_ack.extension_id != EXTENSION_ID
            || runtime_ack.loaded_build_id != loaded_build_id
            || runtime_ack.deployed_build_id != EXPECTED_EXTENSION_BUILD_ID
            || !(250..=5_000).contains(&runtime_ack.reload_delay_ms)
            || runtime_ack.foreground_api_calls != 0
            || runtime_ack.tab_mutations != 0
            || runtime_ack.synthetic_input_events != 0
        {
            return Err(ChromeDebuggerBridgeError::host_reload_failed(format!(
                "SYNAPSE_CHROME_BACKGROUND_RELOAD_ACK_INVALID ok={} control_surface={} required_foreground={} extension_id={} expected_extension_id={} loaded_build_id={} expected_loaded_build_id={} deployed_build_id={} expected_deployed_build_id={} scheduled_at_unix_ms={} reload_delay_ms={} foreground_api_calls={} tab_mutations={} synthetic_input_events={} remediation=repair reloadSelf so it schedules only chrome.runtime.reload and reports exact zero human-window mutation evidence",
                runtime_ack.ok,
                runtime_ack.control_surface,
                runtime_ack.required_foreground,
                runtime_ack.extension_id,
                EXTENSION_ID,
                runtime_ack.loaded_build_id,
                loaded_build_id,
                runtime_ack.deployed_build_id,
                EXPECTED_EXTENSION_BUILD_ID,
                runtime_ack.scheduled_at_unix_ms,
                runtime_ack.reload_delay_ms,
                runtime_ack.foreground_api_calls,
                runtime_ack.tab_mutations,
                runtime_ack.synthetic_input_events
            )));
        }
        let command_ack = ChromeBridgeReloadCommandAck {
            ok: runtime_ack.ok,
            control_surface: runtime_ack.control_surface.clone(),
            required_foreground: runtime_ack.required_foreground,
            installer_path: deploy.installer_path,
            installer_sha256: deploy.installer_sha256,
            installer_exit_code: deploy.installer_exit_code,
            installer_stdout_sha256: deploy.installer_stdout_sha256,
            installer_stderr_sha256: deploy.installer_stderr_sha256,
            installer_duration_ms: deploy.installer_duration_ms,
            extension_id: runtime_ack.extension_id.clone(),
            extension_dir: deploy.extension_dir,
            extension_service_worker_sha256: deploy.extension_service_worker_sha256.clone(),
            active_profile: deploy.active_profile,
            reason: "background_runtime_reload_scheduled_and_verified".to_owned(),
            profile_before_installed: deploy.profile_before_installed,
            profile_before_ready: deploy.profile_before_ready,
            profile_after_installed: deploy.profile_after_installed,
            profile_after_ready: deploy.profile_after_ready,
            loaded_build_id: runtime_ack.loaded_build_id,
            deployed_build_id: runtime_ack.deployed_build_id,
            scheduled_at_unix_ms: runtime_ack.scheduled_at_unix_ms,
            reload_delay_ms: runtime_ack.reload_delay_ms,
            foreground_api_calls: runtime_ack.foreground_api_calls,
            tab_mutations: runtime_ack.tab_mutations,
            synthetic_input_events: runtime_ack.synthetic_input_events,
        };

        let started = Instant::now();
        let mut last_observed = "no_replacement_chrome_bridge_host".to_owned();
        loop {
            if started.elapsed() >= wait_timeout {
                return Err(ChromeDebuggerBridgeError::host_reload_failed(format!(
                    "SYNAPSE_CHROME_BACKGROUND_RELOAD_TIMEOUT before_host_id={} loaded_build_id={} deployed_build_id={} deployed_worker_sha256={} wait_timeout_ms={} last_observed={} remediation=chrome.runtime.reload was acknowledged without foreground mutation, but the daemon did not independently observe the expected replacement host and physical bytes; inspect the extension service-worker console and daemon chrome_bridge health",
                    before.host_id,
                    loaded_build_id,
                    EXPECTED_EXTENSION_BUILD_ID,
                    deploy.extension_service_worker_sha256,
                    wait_timeout_ms,
                    last_observed
                )));
            }
            match self.active_host_snapshot() {
                Ok(after) => {
                    let replacement_host = after.host_id != before.host_id;
                    let expected_identity = after.extension_id.as_deref() == Some(EXTENSION_ID)
                        && after.extension_build_id.as_deref() == Some(EXPECTED_EXTENSION_BUILD_ID)
                        && after.extension_declared_build_sha256.as_deref()
                            == Some(EXPECTED_EXTENSION_DECLARED_BUILD_SHA256)
                        && after.extension_service_worker_sha256.as_deref()
                            == Some(deploy.extension_service_worker_sha256.as_str());
                    last_observed = format!(
                        "host_id={} replacement_host={} extension_id={} build_id={} declared_sha256={} worker_sha256={} stale={} stale_reasons={}",
                        after.host_id,
                        replacement_host,
                        after.extension_id.as_deref().unwrap_or("<missing>"),
                        after.extension_build_id.as_deref().unwrap_or("<missing>"),
                        after
                            .extension_declared_build_sha256
                            .as_deref()
                            .unwrap_or("<missing>"),
                        after
                            .extension_service_worker_sha256
                            .as_deref()
                            .unwrap_or("<missing>"),
                        after.extension_stale,
                        after.extension_stale_reasons.join("|")
                    );
                    if replacement_host && expected_identity && after.extension_stale {
                        return Err(ChromeDebuggerBridgeError::host_reload_failed(format!(
                            "SYNAPSE_CHROME_BACKGROUND_RELOAD_REPLACEMENT_UNHEALTHY host_id={} stale_reasons={} startup_readback={} remediation=the replacement worker loaded the expected physical bytes but failed a durable startup gate; repair the exact named gate before retrying",
                            after.host_id,
                            after.extension_stale_reasons.join("|"),
                            after
                                .extension_startup_readback
                                .as_ref()
                                .map_or_else(|| "<missing>".to_owned(), ToString::to_string)
                        )));
                    }
                    if replacement_host
                        && expected_identity
                        && !after.extension_stale
                        && after.last_disconnect_detail.is_none()
                    {
                        invalidate_chrome_profile_scan_cache(
                            "background_reload_replacement_host_readback",
                        );
                        let final_profile = synapse_chrome_profile_install_state();
                        let final_worker_sha256 = final_profile
                            .active_profile_service_worker_sha256
                            .as_deref()
                            .unwrap_or("<missing>");
                        let final_profile_installed =
                            final_profile.active_profile_extension_path.is_some();
                        if !final_profile_installed
                            || final_worker_sha256 != deploy.extension_service_worker_sha256
                        {
                            return Err(ChromeDebuggerBridgeError::host_reload_failed(format!(
                                "SYNAPSE_CHROME_BACKGROUND_RELOAD_FINAL_PROFILE_READBACK_FAILED active_profile_installed={} expected_worker_sha256={} actual_worker_sha256={} profile_error={} remediation=the replacement host is healthy, but a separate final physical profile/filesystem read drifted; repair the stable extension row before accepting reload",
                                final_profile_installed,
                                deploy.extension_service_worker_sha256,
                                final_worker_sha256,
                                final_profile
                                    .active_profile_service_worker_error
                                    .as_deref()
                                    .unwrap_or("<none>")
                            )));
                        }
                        let waited_ms =
                            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                        tracing::info!(
                            code = "CHROME_DEBUGGER_BACKGROUND_RUNTIME_RELOAD_VERIFIED",
                            before_host_id = %before.host_id,
                            after_host_id = %after.host_id,
                            extension_build_id = EXPECTED_EXTENSION_BUILD_ID,
                            extension_service_worker_sha256 = %deploy.extension_service_worker_sha256,
                            required_foreground = false,
                            foreground_api_calls = 0,
                            tab_mutations = 0,
                            synthetic_input_events = 0,
                            "replacement Chrome bridge host and physical profile bytes verified after chrome.runtime.reload"
                        );
                        return Ok(ChromeBridgeReloadResult {
                            before: Some(before),
                            command_ack,
                            after,
                            reconnected: true,
                            waited_ms,
                        });
                    }
                }
                Err(error) => {
                    last_observed = format!(
                        "host_read_error_code={} detail_sha256={}",
                        error.code(),
                        sha256_hex_lower(error.detail().as_bytes())
                    );
                }
            }
            sleep(RELOAD_RECONNECT_POLL_INTERVAL).await;
        }
    }

    async fn maintenance_pause_reconnect(
        &self,
        reason: String,
        pause_ms: u64,
        resume_probe_after_ms: u64,
    ) -> Result<ChromeBridgeMaintenancePauseAck, ChromeDebuggerBridgeError> {
        let daemon_pid = std::process::id();
        let daemon_instance_id = daemon_instance_id().to_owned();
        let result = self
            .send_command_with_timeout(
                MAINTENANCE_RECONNECT_PAUSE_COMMAND,
                json!({
                    "pauseMs": pause_ms,
                    "resumeProbeAfterMs": resume_probe_after_ms,
                    "reason": reason,
                    "requestedAtUnixMs": now_unix_ms(),
                    "pausedDaemonPid": daemon_pid,
                    "pausedDaemonInstanceId": daemon_instance_id,
                }),
                Duration::from_secs(5),
            )
            .await?;
        let mut ack =
            serde_json::from_value::<ChromeBridgeMaintenancePauseAck>(result).map_err(|error| {
                ChromeDebuggerBridgeError::protocol(format!(
                    "decode Chrome bridge maintenancePauseReconnect acknowledgement: {error}"
                ))
            })?;
        let active_socket_was_open = matches!(ack.websocket_close.ready_state_before, Some(0 | 1));
        let websocket_close_failed = ack.websocket_close.close_error.is_some()
            || (active_socket_was_open
                && !ack.websocket_close.close_requested
                && !ack.websocket_close.close_deferred);
        if !ack.ok
            || !ack.reconnect_suppressed
            || !ack.persisted
            || websocket_close_failed
            || ack.pause_ms != pause_ms
            || ack.resume_probe_after_ms != resume_probe_after_ms
            || ack.paused_daemon_pid != Some(daemon_pid)
            || ack.paused_daemon_instance_id.as_deref() != Some(daemon_instance_id.as_str())
            || ack.reason.trim() != reason.trim()
        {
            return Err(ChromeDebuggerBridgeError::protocol(format!(
                "Chrome bridge maintenancePauseReconnect acknowledgement failed postcondition: ok={} reconnect_suppressed={} persisted={} websocket_had_socket={} websocket_ready_state_before={:?} websocket_close_requested={} websocket_close_deferred={} websocket_close_error={:?} pause_ms={} expected_pause_ms={} resume_probe_after_ms={} expected_resume_probe_after_ms={} paused_daemon_pid={:?} expected_paused_daemon_pid={} paused_daemon_instance_id={:?} expected_paused_daemon_instance_id={} reason={:?} expected_reason={:?}",
                ack.ok,
                ack.reconnect_suppressed,
                ack.persisted,
                ack.websocket_close.had_socket,
                ack.websocket_close.ready_state_before,
                ack.websocket_close.close_requested,
                ack.websocket_close.close_deferred,
                ack.websocket_close.close_error,
                ack.pause_ms,
                pause_ms,
                ack.resume_probe_after_ms,
                resume_probe_after_ms,
                ack.paused_daemon_pid,
                daemon_pid,
                ack.paused_daemon_instance_id,
                daemon_instance_id,
                ack.reason,
                reason
            )));
        }
        if active_socket_was_open
            && let Some(host_id) = ack.host_id.as_deref().filter(|host_id| !host_id.is_empty())
        {
            self.disconnect_direct_http_host(
                host_id,
                "maintenance pause acknowledged; daemon closing direct HTTP WebSocket before restart",
            );
            ack.daemon_disconnect_requested = true;
            ack.daemon_disconnect_host_id = Some(host_id.to_owned());
        }
        Ok(ack)
    }

    async fn wait_for_active_host(
        &self,
        wait_timeout: Duration,
    ) -> Result<ChromeBridgeHostSnapshot, ChromeDebuggerBridgeError> {
        let started = Instant::now();
        loop {
            if let Ok(snapshot) = self.active_host_snapshot()
                && snapshot.extension_id.as_deref() == Some(EXTENSION_ID)
                && snapshot.last_disconnect_detail.is_none()
            {
                if snapshot.extension_stale {
                    return Err(ChromeDebuggerBridgeError {
                        code: error_codes::CHROME_BRIDGE_EXTENSION_STALE,
                        detail: format!(
                            "Chrome bridge active host is stale after reconnect wait; host_id={} stale_reasons={}",
                            snapshot.host_id,
                            snapshot.extension_stale_reasons.join("|")
                        ),
                    });
                }
                return Ok(snapshot);
            }
            if started.elapsed() >= wait_timeout {
                return Err(ChromeDebuggerBridgeError::timeout(
                    "waitActiveChromeBridgeHost",
                ));
            }
            sleep(RELOAD_RECONNECT_POLL_INTERVAL).await;
        }
    }

    async fn next_command(
        &self,
        host_id: &str,
        timeout_duration: Duration,
    ) -> Result<Option<ChromeCommand>, String> {
        let started = tokio::time::Instant::now();
        loop {
            let notified = {
                let mut inner = self.inner.lock().map_err(|_| {
                    "chrome debugger bridge lock poisoned during next command".to_owned()
                })?;
                if !inner.hosts.contains_key(host_id) {
                    return Err(format!(
                        "unknown chrome debugger native host_id {host_id:?}"
                    ));
                }
                if let Some(host) = inner.hosts.get_mut(host_id) {
                    host.last_seen_unix_ms = now_unix_ms();
                }
                if let Some(index) = inner
                    .commands
                    .iter()
                    .position(|queued| queued.host_id == host_id)
                {
                    let queued = inner
                        .commands
                        .remove(index)
                        .ok_or_else(|| "queued command disappeared".to_owned())?;
                    if let Some(pending) = inner.pending.get_mut(&queued.command.id) {
                        pending.delivered = true;
                    }
                    tracing::info!(
                        code = "CHROME_DEBUGGER_COMMAND_DELIVERED",
                        host_id = %host_id,
                        command_id = %queued.command.id,
                        command_kind = %queued.command.kind,
                        "Chrome debugger command delivered to bridge host"
                    );
                    return Ok(Some(queued.command));
                }
                self.notify.notified()
            };
            let elapsed = started.elapsed();
            if elapsed >= timeout_duration {
                return Ok(None);
            }
            let remaining = timeout_duration.saturating_sub(elapsed);
            if timeout(remaining, notified).await.is_err() {
                return Ok(None);
            }
        }
    }

    async fn send_command(
        &self,
        kind: &str,
        params: Value,
    ) -> Result<Value, ChromeDebuggerBridgeError> {
        self.send_command_with_timeout_policy(kind, params, COMMAND_TIMEOUT)
            .await
    }

    /// Like `send_command` but with an explicit daemon-side response budget.
    /// Long-running in-extension operations (e.g. `downloads` operation=wait with
    /// a caller `waitTimeoutMs` greater than the default 30s) must give the daemon
    /// a budget that outlives the in-page wait, otherwise the daemon kills the
    /// command first and surfaces a transport-looking `A11Y_CDP_EXTENSION_TIMEOUT`
    /// instead of the extension's clean no-match result (#1342).
    async fn send_command_with_timeout(
        &self,
        kind: &str,
        params: Value,
        command_timeout: Duration,
    ) -> Result<Value, ChromeDebuggerBridgeError> {
        self.send_command_with_timeout_policy(kind, params, command_timeout)
            .await
    }

    async fn send_command_with_timeout_policy(
        &self,
        kind: &str,
        params: Value,
        command_timeout: Duration,
    ) -> Result<Value, ChromeDebuggerBridgeError> {
        let id = format!(
            "chrome-cdp-{}-{}",
            std::process::id(),
            self.command_seq.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        let command = ChromeCommand {
            id: id.clone(),
            kind: kind.to_owned(),
            params,
        };
        {
            let mut inner = self.inner.lock().map_err(|_| {
                ChromeDebuggerBridgeError::protocol(
                    "chrome debugger bridge lock poisoned during command enqueue",
                )
            })?;
            let Some(host_id) = inner.active_host_id.clone() else {
                return Err(ChromeDebuggerBridgeError::unavailable());
            };
            if !inner.hosts.contains_key(&host_id) {
                inner.active_host_id = None;
                inner.host_lineage_sequence = inner.host_lineage_sequence.saturating_add(1);
                return Err(ChromeDebuggerBridgeError::unavailable());
            }
            let host = inner
                .hosts
                .get(&host_id)
                .ok_or_else(ChromeDebuggerBridgeError::unavailable)?;
            if let Some(reason) = bridge_command_stale_reason(host, kind) {
                let error = ChromeDebuggerBridgeError::stale(kind, &host_id, host, &reason);
                tracing::warn!(
                    code = error.code(),
                    host_id = %host_id,
                    command_kind = %kind,
                    detail = %error.detail(),
                    "Chrome debugger command refused before enqueue because loaded extension bridge is stale"
                );
                return Err(error);
            }
            let transport_label = host
                .transport
                .as_deref()
                .unwrap_or("native_messaging")
                .to_owned();
            inner.pending.insert(
                id.clone(),
                PendingResponse {
                    host_id: host_id.clone(),
                    kind: kind.to_owned(),
                    sender: Some(sender),
                    mutation_capable: kind != MAINTENANCE_RECONNECT_PAUSE_COMMAND,
                    delivered: false,
                    caller_timed_out: false,
                    transport_lost: false,
                },
            );
            inner.commands.push_back(QueuedCommand {
                host_id: host_id.clone(),
                command,
            });
            tracing::info!(
                code = "CHROME_DEBUGGER_COMMAND_QUEUED",
                host_id = %host_id,
                command_id = %id,
                command_kind = %kind,
                transport = %transport_label,
                queue_depth = inner.commands.len(),
                "Chrome debugger command queued for bridge host"
            );
        }
        self.notify.notify_waiters();

        let response = match timeout(command_timeout, receiver).await {
            Ok(Ok(response)) => response,
            Ok(Err(_closed)) => {
                self.abandon_pending_caller(&id, "response_channel_closed");
                return Err(ChromeDebuggerBridgeError::protocol(format!(
                    "Chrome debugger command {kind:?} response channel closed"
                )));
            }
            Err(_elapsed) => {
                self.abandon_pending_caller(&id, "caller_timeout");
                return Err(ChromeDebuggerBridgeError::timeout(kind));
            }
        };
        if response.ok {
            return response.result.ok_or_else(|| {
                ChromeDebuggerBridgeError::protocol(format!(
                    "Chrome debugger command {kind:?} returned ok without result"
                ))
            });
        }
        let error = response.error.as_ref();
        Err(ChromeDebuggerBridgeError::extension(
            error.and_then(|error| error.code.as_deref()),
            error
                .and_then(|error| error.detail.clone())
                .unwrap_or_else(|| format!("Chrome debugger command {kind:?} failed")),
        ))
    }

    fn abandon_pending_caller(&self, id: &str, reason: &'static str) {
        let Ok(mut inner) = self.inner.lock() else {
            synapse_action::record_operator_panic_safety_incident();
            return;
        };
        let keep_delivered_command = inner
            .pending
            .get(id)
            .is_some_and(|pending| pending.delivered);
        if keep_delivered_command {
            if let Some(pending) = inner.pending.get_mut(id) {
                pending.sender = None;
                pending.caller_timed_out = true;
                tracing::error!(
                    code = "CHROME_DEBUGGER_DELIVERED_COMMAND_CALLER_TIMED_OUT",
                    command_id = %id,
                    command_kind = %pending.kind,
                    reason,
                    "delivered Chrome command remains owned until its durable terminal is acknowledged"
                );
            }
            return;
        }
        inner.commands.retain(|queued| queued.command.id != id);
        if let Some(pending) = inner.pending.remove(id) {
            tracing::warn!(
                code = "CHROME_DEBUGGER_PENDING_DROPPED_BEFORE_DELIVERY",
                command_id = %id,
                command_kind = %pending.kind,
                reason,
                "Chrome debugger command ownership ended before delivery"
            );
        }
    }

    fn direct_http_bridge_token_matches(&self, token: &str) -> bool {
        let token = token.trim();
        if token.is_empty() {
            return false;
        }
        let candidate = digest_bridge_token(token);
        self.inner.lock().is_ok_and(|inner| {
            inner.hosts.values().any(|host| {
                host.transport.as_deref() == Some("direct_http")
                    && bool::from(
                        host.bridge_token_digest
                            .as_slice()
                            .ct_eq(candidate.as_slice()),
                    )
            })
        })
    }

    fn direct_http_bridge_token_matches_host(&self, host_id: &str, token: &str) -> bool {
        let token = token.trim();
        if token.is_empty() {
            return false;
        }
        let candidate = digest_bridge_token(token);
        self.inner.lock().is_ok_and(|inner| {
            inner.hosts.get(host_id).is_some_and(|host| {
                host.transport.as_deref() == Some("direct_http")
                    && bool::from(
                        host.bridge_token_digest
                            .as_slice()
                            .ct_eq(candidate.as_slice()),
                    )
            })
        })
    }

    fn touch_host(&self, host_id: &str) -> bool {
        let now = now_unix_ms();
        self.inner.lock().is_ok_and(|mut inner| {
            inner.hosts.get_mut(host_id).is_some_and(|host| {
                if host.transport.as_deref() != Some("direct_http") {
                    return false;
                }
                host.last_seen_unix_ms = now;
                true
            })
        })
    }

    fn accept_command_terminal(&self, socket_host_id: &str, raw: &str) -> Value {
        let nack = |command_id: &str, code: &str, detail: String| {
            tracing::error!(
                code,
                host_id = %socket_host_id,
                command_id,
                detail = %detail,
                "Chrome command terminal rejected"
            );
            json!({
                "type": "command_terminal_nack",
                "ok": false,
                "code": code,
                "command_id": command_id,
                "detail": detail,
            })
        };
        let envelope = match serde_json::from_str::<CommandTerminalEnvelope>(raw) {
            Ok(envelope) => envelope,
            Err(error) => {
                return nack(
                    "",
                    error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                    format!("decode command terminal envelope: {error}"),
                );
            }
        };
        let command_id = envelope.command_id.clone();
        if envelope.message_type != "command_terminal"
            || envelope.protocol_version != BRIDGE_PROTOCOL_VERSION
            || envelope.original_host_id.is_empty()
            || envelope.browser_session_id.is_empty()
            || envelope.command_id.is_empty()
            || envelope.command_kind.is_empty()
            || envelope.terminal_sequence == 0
            || envelope.payload_bytes == 0
            || envelope.payload_bytes > COMMAND_TERMINAL_PAYLOAD_BUDGET_BYTES
            || envelope.created_at_unix_ms == 0
            || envelope.worker_boot_id.is_empty()
        {
            return nack(
                &command_id,
                error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                format!(
                    "invalid command terminal fields protocol_version={} original_host_id={:?} browser_session_id_present={} command_kind={:?} terminal_sequence={} payload_bytes={} payload_limit_bytes={} created_at_unix_ms={} worker_boot_id_present={}",
                    envelope.protocol_version,
                    envelope.original_host_id,
                    !envelope.browser_session_id.is_empty(),
                    envelope.command_kind,
                    envelope.terminal_sequence,
                    envelope.payload_bytes,
                    COMMAND_TERMINAL_PAYLOAD_BUDGET_BYTES,
                    envelope.created_at_unix_ms,
                    !envelope.worker_boot_id.is_empty(),
                ),
            );
        }
        let actual_payload_bytes = envelope.payload_json.len();
        let actual_payload_sha256 = sha256_hex_lower(envelope.payload_json.as_bytes());
        if actual_payload_bytes != envelope.payload_bytes
            || actual_payload_sha256 != envelope.payload_sha256
        {
            return nack(
                &command_id,
                error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                format!(
                    "command terminal payload evidence mismatch recorded_bytes={} actual_bytes={} recorded_sha256={} actual_sha256={}",
                    envelope.payload_bytes,
                    actual_payload_bytes,
                    envelope.payload_sha256,
                    actual_payload_sha256,
                ),
            );
        }
        let response = match serde_json::from_str::<ChromeResponse>(&envelope.payload_json) {
            Ok(response) => response,
            Err(error) => {
                return nack(
                    &command_id,
                    error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                    format!("decode command terminal response payload: {error}"),
                );
            }
        };
        if response.id != command_id {
            return nack(
                &command_id,
                error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                format!(
                    "command terminal response id mismatch envelope_id={command_id:?} response_id={:?}",
                    response.id
                ),
            );
        }

        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => {
                return nack(
                    &command_id,
                    error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                    "chrome debugger bridge lock poisoned during terminal acceptance".to_owned(),
                );
            }
        };
        if inner.active_host_id.as_deref() != Some(socket_host_id)
            || inner
                .hosts
                .get(socket_host_id)
                .and_then(|host| host.transport.as_deref())
                != Some("direct_http")
        {
            return nack(
                &command_id,
                error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                "command terminal arrived on a non-active or non-direct authenticated host"
                    .to_owned(),
            );
        }
        if let Some(receipt) = inner
            .command_terminal_receipts
            .iter()
            .find(|receipt| receipt.command_id == command_id)
        {
            let exact = receipt.command_kind == envelope.command_kind
                && receipt.original_host_id == envelope.original_host_id
                && receipt.browser_session_id == envelope.browser_session_id
                && receipt.terminal_sequence == envelope.terminal_sequence
                && receipt.payload_sha256 == envelope.payload_sha256
                && receipt.payload_bytes == envelope.payload_bytes;
            if !exact {
                return nack(
                    &command_id,
                    error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                    "replayed command terminal contradicts the daemon receipt ledger".to_owned(),
                );
            }
            tracing::info!(
                code = "CHROME_DEBUGGER_COMMAND_TERMINAL_DEDUPLICATED",
                host_id = %socket_host_id,
                command_id = %command_id,
                terminal_sequence = envelope.terminal_sequence,
                payload_sha256 = %envelope.payload_sha256,
                payload_bytes = envelope.payload_bytes,
                "matching durable Chrome command terminal replay acknowledged without re-execution"
            );
            return json!({
                "type": "command_terminal_ack",
                "ok": true,
                "host_id": socket_host_id,
                "command_id": command_id,
                "terminal_sequence": envelope.terminal_sequence,
                "payload_sha256": envelope.payload_sha256,
                "payload_bytes": envelope.payload_bytes,
                "deduplicated": true,
            });
        }
        let Some(pending) = inner.pending.get(&command_id) else {
            return nack(
                &command_id,
                error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                "command terminal has neither a pending delivered command nor a matching receipt"
                    .to_owned(),
            );
        };
        if !pending.delivered
            || pending.host_id != envelope.original_host_id
            || pending.kind != envelope.command_kind
        {
            return nack(
                &command_id,
                error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                format!(
                    "command terminal ownership mismatch delivered={} expected_host_id={:?} actual_host_id={:?} expected_kind={:?} actual_kind={:?}",
                    pending.delivered,
                    pending.host_id,
                    envelope.original_host_id,
                    pending.kind,
                    envelope.command_kind,
                ),
            );
        }
        let high_water = inner
            .command_terminal_high_water
            .get(&envelope.browser_session_id)
            .copied()
            .unwrap_or(0);
        if envelope.terminal_sequence <= high_water {
            return nack(
                &command_id,
                error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                format!(
                    "new command terminal sequence is not above the browser-session high-water mark; sequence={} high_water={high_water}",
                    envelope.terminal_sequence
                ),
            );
        }
        let Some(pending) = inner.pending.remove(&command_id) else {
            return nack(
                &command_id,
                error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                "pending command disappeared while accepting its terminal".to_owned(),
            );
        };
        inner.command_terminal_high_water.insert(
            envelope.browser_session_id.clone(),
            envelope.terminal_sequence,
        );
        inner
            .command_terminal_receipts
            .push_back(CommandTerminalReceipt {
                command_id: command_id.clone(),
                command_kind: envelope.command_kind.clone(),
                original_host_id: envelope.original_host_id.clone(),
                browser_session_id: envelope.browser_session_id.clone(),
                terminal_sequence: envelope.terminal_sequence,
                payload_sha256: envelope.payload_sha256.clone(),
                payload_bytes: envelope.payload_bytes,
            });
        while inner.command_terminal_receipts.len() > COMMAND_TERMINAL_RECEIPT_LIMIT {
            inner.command_terminal_receipts.pop_front();
        }
        let readback_summary =
            chrome_response_readback_summary(&pending.kind, response.result.as_ref());
        let response_error = response.error.as_ref();
        let response_error_diagnostics =
            response_error.and_then(|error| error.diagnostics.as_ref());
        tracing::info!(
            code = "CHROME_DEBUGGER_COMMAND_TERMINAL_ACCEPTED",
            host_id = %socket_host_id,
            original_host_id = %envelope.original_host_id,
            command_id = %command_id,
            command_kind = %pending.kind,
            terminal_sequence = envelope.terminal_sequence,
            payload_sha256 = %envelope.payload_sha256,
            payload_bytes = envelope.payload_bytes,
            response_ok = response.ok,
            response_error_code = response_error
                .and_then(|error| error.code.as_deref())
                .unwrap_or(""),
            response_error_detail = response_error
                .and_then(|error| error.detail.as_deref())
                .unwrap_or(""),
            exception_type = response_error_diagnostics
                .map(|diagnostics| diagnostics.exception_type.as_str())
                .unwrap_or(""),
            exception_message = response_error_diagnostics
                .map(|diagnostics| diagnostics.exception_message.as_str())
                .unwrap_or(""),
            exception_line_number = response_error_diagnostics
                .and_then(|diagnostics| diagnostics.line_number)
                .unwrap_or(0),
            exception_column_number = response_error_diagnostics
                .and_then(|diagnostics| diagnostics.column_number)
                .unwrap_or(0),
            readback = %readback_summary.as_deref().unwrap_or(""),
            "durable Chrome command terminal accepted on the authoritative WebSocket"
        );
        if let Some(sender) = pending.sender {
            let _ = sender.send(response);
        }
        json!({
            "type": "command_terminal_ack",
            "ok": true,
            "host_id": socket_host_id,
            "command_id": command_id,
            "terminal_sequence": envelope.terminal_sequence,
            "payload_sha256": envelope.payload_sha256,
            "payload_bytes": envelope.payload_bytes,
            "deduplicated": false,
        })
    }

    fn disconnect_direct_http_host(&self, host_id: &str, detail: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            tracing::error!(
                code = "CHROME_DEBUGGER_DIRECT_HTTP_WS_DISCONNECT_LOCK_POISONED",
                host_id = %host_id,
                detail = %detail,
                "Chrome debugger direct HTTP bridge disconnect could not acquire lock"
            );
            return;
        };
        if inner
            .hosts
            .get(host_id)
            .and_then(|host| host.transport.as_deref())
            != Some("direct_http")
        {
            return;
        }
        if let Some(host) = inner.hosts.get_mut(host_id) {
            host.last_disconnect_detail = Some(detail.to_owned());
        }
        if inner.active_host_id.as_deref() == Some(host_id) {
            inner.active_host_id = None;
            inner.host_lineage_sequence = inner.host_lineage_sequence.saturating_add(1);
        }
        let terminal_detail = format!(
            "Chrome debugger direct HTTP bridge host {host_id} disconnected before command response: {detail}"
        );
        let stats =
            preserve_delivered_mutations_on_transport_loss(&mut inner, host_id, &terminal_detail);
        inner.hosts.remove(host_id);
        tracing::warn!(
            code = "CHROME_DEBUGGER_DIRECT_HTTP_WS_DISCONNECTED",
            host_id = %host_id,
            detail = %detail,
            queued_removed = stats.queued_removed,
            pending_failed_before_delivery = stats.pending_failed_before_delivery,
            delivered_commands_preserved = stats.delivered_commands_preserved,
            "Chrome debugger direct HTTP WebSocket disconnected"
        );
        self.notify.notify_waiters();
    }
}

fn bridge() -> &'static ChromeDebuggerBridge {
    static BRIDGE: OnceLock<ChromeDebuggerBridge> = OnceLock::new();
    BRIDGE.get_or_init(|| ChromeDebuggerBridge {
        inner: Mutex::new(BridgeInner::default()),
        notify: Notify::new(),
        command_seq: AtomicU64::new(1),
    })
}

#[must_use]
pub fn mutation_command_owners_readback() -> ChromeDebuggerMutationOwnersReadback {
    let Ok(inner) = bridge().inner.lock() else {
        synapse_action::record_operator_panic_safety_incident();
        return ChromeDebuggerMutationOwnersReadback {
            queued_mutation_count: usize::MAX,
            delivered_mutation_count: usize::MAX,
            delivered_caller_timed_out_count: usize::MAX,
            delivered_transport_lost_count: usize::MAX,
            registry_readback_healthy: false,
            registry_readback_failure: Some(
                "Chrome debugger mutation-owner registry lock is poisoned".to_owned(),
            ),
            ..Default::default()
        };
    };
    let queued_mutation_count = inner
        .pending
        .values()
        .filter(|pending| pending.mutation_capable && !pending.delivered)
        .count();
    let mut delivered = inner
        .pending
        .iter()
        .filter(|(_, pending)| pending.mutation_capable && pending.delivered)
        .map(|(id, pending)| (id.clone(), pending.caller_timed_out, pending.transport_lost))
        .collect::<Vec<_>>();
    delivered.sort_by(|left, right| left.0.cmp(&right.0));
    ChromeDebuggerMutationOwnersReadback {
        queued_mutation_count,
        delivered_mutation_count: delivered.len(),
        delivered_caller_timed_out_count: delivered
            .iter()
            .filter(|(_, caller_timed_out, _)| *caller_timed_out)
            .count(),
        delivered_transport_lost_count: delivered
            .iter()
            .filter(|(_, _, transport_lost)| *transport_lost)
            .count(),
        delivered_command_ids: delivered.into_iter().map(|(id, _, _)| id).collect(),
        registry_readback_healthy: true,
        registry_readback_failure: None,
    }
}

fn resolve_abandoned_delivered_mutations_after_extension_empty_readback(
    readback: &ChromeDebuggerExtensionOwnerReadback,
    authoritative_host_id: &str,
) -> Result<usize, ChromeDebuggerBridgeError> {
    if !extension_owner_readback_is_authoritative_empty(readback) {
        return Ok(0);
    }
    let mut inner = bridge().inner.lock().map_err(|_| {
        synapse_action::record_operator_panic_safety_incident();
        ChromeDebuggerBridgeError::protocol(
            "Chrome debugger mutation-owner registry lock poisoned while reconciling authoritative extension readback",
        )
    })?;
    let resolved_ids = inner
        .pending
        .iter()
        .filter(|(_id, pending)| {
            pending.mutation_capable
                && pending.delivered
                && pending.sender.is_none()
                && pending.caller_timed_out
                && !pending.transport_lost
                && pending.host_id == authoritative_host_id
        })
        .map(|(id, _pending)| id.clone())
        .collect::<Vec<_>>();
    for id in &resolved_ids {
        inner.pending.remove(id);
    }
    if !resolved_ids.is_empty() {
        tracing::warn!(
            code = "CHROME_DEBUGGER_ABANDONED_MUTATIONS_RESOLVED_BY_EXTENSION_READBACK",
            disable_sequence = readback.disable_sequence,
            resolved_count = resolved_ids.len(),
            resolved_command_ids = ?resolved_ids,
            "authoritative serialized extension cleanup/readback proved abandoned delivered mutations terminal"
        );
    }
    Ok(resolved_ids.len())
}

fn extension_owner_readback_is_authoritative_empty(
    readback: &ChromeDebuggerExtensionOwnerReadback,
) -> bool {
    !readback.enabled
        && readback.fully_drained
        && readback.command_in_flight_count == 1
        && readback.command_activity_sequence
            == readback.command_last_completed_sequence.saturating_add(1)
        && readback.mutation_handlers_started_count == readback.mutation_handlers_completed_count
        && readback.storage_state_loaded
        && readback.storage_state_load_error.is_none()
        && readback.browser_session_continuity_matched
        && readback.stale_browser_session_owner_count == 0
        && readback
            .browser_session_id
            .as_deref()
            .is_some_and(|session| !session.is_empty())
        && readback.browser_session_id.as_deref()
            == Some(readback.ledger_browser_session_id.as_str())
        && readback.persisted_in_flight_mutation.is_none()
        && readback.unresolved_debugger_command_timeouts.is_empty()
        && readback.unresolved_worker_restart_mutation_count == 0
        && readback.owner_continuity_healthy
        && readback.active_after.is_empty()
}

fn active_bridge_host_lineage_snapshot() -> Result<(Option<String>, u64), ChromeDebuggerBridgeError>
{
    bridge()
        .inner
        .lock()
        .map(|inner| (inner.active_host_id.clone(), inner.host_lineage_sequence))
        .map_err(|_| {
            synapse_action::record_operator_panic_safety_incident();
            ChromeDebuggerBridgeError::protocol(
                "Chrome debugger bridge lock poisoned while reading active host lineage",
            )
        })
}

/// Cancels commands which have not crossed the extension delivery boundary,
/// then waits for every delivered mutation to publish a terminal response.
/// Transport teardown deliberately preserves delivered ownership until daemon
/// owner-registry teardown; a replacement transport lineage cannot reconcile
/// it. Only caller-timeout ownership on the same continuous host lineage may be
/// reconciled by serialized cleanup plus an independent empty readback. A
/// timeout is a fail-closed unresolved verdict, never terminal ownership.
pub async fn drain_mutation_command_owners(
    wait_timeout: Duration,
) -> ChromeDebuggerMutationOwnersDrainReadback {
    let before = mutation_command_owners_readback();
    if !before.registry_readback_healthy {
        return ChromeDebuggerMutationOwnersDrainReadback {
            failures: before
                .registry_readback_failure
                .clone()
                .into_iter()
                .collect(),
            readback: before,
            fully_drained: false,
            ..Default::default()
        };
    }
    let queued_mutations_canceled = {
        let Ok(mut inner) = bridge().inner.lock() else {
            synapse_action::record_operator_panic_safety_incident();
            return ChromeDebuggerMutationOwnersDrainReadback {
                failures: vec![
                    "Chrome debugger mutation-owner registry lock poisoned during drain".to_owned(),
                ],
                readback: mutation_command_owners_readback(),
                fully_drained: false,
                ..Default::default()
            };
        };
        let queued_ids = inner
            .pending
            .iter()
            .filter(|(_id, pending)| pending.mutation_capable && !pending.delivered)
            .map(|(id, _pending)| id.clone())
            .collect::<Vec<_>>();
        inner
            .commands
            .retain(|queued| !queued_ids.iter().any(|id| id == &queued.command.id));
        for id in &queued_ids {
            if let Some(pending) = inner.pending.remove(id)
                && let Some(sender) = pending.sender
            {
                let _ = sender.send(ChromeResponse {
                    id: id.clone(),
                    ok: false,
                    result: None,
                    error: Some(ChromeResponseError {
                        code: Some(error_codes::SAFETY_OPERATOR_HOTKEY_FIRED.to_owned()),
                        detail: Some(
                            "operator panic canceled Chrome command before extension delivery"
                                .to_owned(),
                        ),
                        diagnostics: None,
                    }),
                });
            }
        }
        queued_ids.len()
    };

    let started = Instant::now();
    loop {
        let readback = mutation_command_owners_readback();
        if !readback.registry_readback_healthy
            || readback.delivered_mutation_count == 0
            || started.elapsed() >= wait_timeout
        {
            let terminal_acks_observed = before
                .delivered_mutation_count
                .saturating_sub(readback.delivered_mutation_count);
            let mut failures = Vec::new();
            if !readback.registry_readback_healthy {
                failures.extend(readback.registry_readback_failure.clone());
            }
            if readback.delivered_mutation_count != 0 {
                failures.push(format!(
                    "{} delivered Chrome mutation command(s) remain unresolved after {} ms: {:?}",
                    readback.delivered_mutation_count,
                    started.elapsed().as_millis(),
                    readback.delivered_command_ids
                ));
            }
            let fully_drained = failures.is_empty()
                && readback.queued_mutation_count == 0
                && readback.delivered_mutation_count == 0;
            if !fully_drained {
                synapse_action::record_operator_panic_safety_incident();
            }
            return ChromeDebuggerMutationOwnersDrainReadback {
                queued_mutations_canceled,
                terminal_acks_observed,
                failures,
                readback,
                fully_drained,
            };
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn health_subsystem() -> SubsystemHealth {
    let profile_scan = chrome_profile_scan();
    let mut popup_risks = profile_scan.scan.external_profile_surfaces.clone();
    popup_risks.extend(external_chrome_native_messaging_processes());
    popup_risks.sort();
    popup_risks.dedup();
    let self_profile_risks = profile_scan.scan.self_profile_surfaces.clone();
    let layout_infobar_risks = external_chrome_layout_infobar_processes();
    let profile_install_state = profile_scan.install_state();
    let snapshot = match bridge().inner.try_lock() {
        Ok(inner) => {
            let active_host = inner
                .active_host_id
                .as_ref()
                .and_then(|host_id| inner.hosts.get(host_id).map(|host| (host_id, host)))
                .map(|(host_id, host)| {
                    host_record_to_health_record_with_profile_state(
                        host_id,
                        host,
                        &profile_install_state,
                    )
                });
            (
                active_host,
                inner.hosts.len(),
                inner.commands.len(),
                inner.pending.len(),
            )
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            return SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(format!(
                    "chrome_bridge_state_lock_busy tab_control_available=false expected_extension_id={EXTENSION_ID}; health is fail-closed and does not wait behind in-flight bridge work"
                )),
                ..SubsystemHealth::default()
            };
        }
        Err(std::sync::TryLockError::Poisoned(_poisoned)) => {
            return SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(format!(
                    "chrome_bridge_state_lock_poisoned tab_control_available=false expected_extension_id={EXTENSION_ID}"
                )),
                ..SubsystemHealth::default()
            };
        }
    };
    let self_policy_shield = synapse_chrome_self_policy_shield_status();
    chrome_bridge_health_from_snapshot_with_self_policy(
        snapshot.0.as_ref(),
        snapshot.1,
        snapshot.2,
        snapshot.3,
        &popup_risks,
        &self_profile_risks,
        &layout_infobar_risks,
        &self_policy_shield,
        &profile_install_state,
    )
}

#[allow(clippy::too_many_arguments)]
fn chrome_bridge_health_from_snapshot_with_self_policy(
    active_host: Option<&ChromeBridgeHealthRecord>,
    host_count: usize,
    queued_count: usize,
    pending_count: usize,
    popup_risks: &[String],
    self_profile_risks: &[String],
    layout_infobar_risks: &[String],
    self_policy_shield: &SynapseChromeSelfPolicyShieldStatus,
    profile_install_state: &SynapseChromeProfileInstallState,
) -> SubsystemHealth {
    let layout_warning = external_chrome_layout_infobar_warning(layout_infobar_risks);
    let self_permission_warning =
        synapse_chrome_self_permission_warning(self_profile_risks, self_policy_shield.present);
    let self_permission_blocking =
        !synapse_chrome_self_active_popup_risks(self_profile_risks).is_empty();

    let Some(host) = active_host else {
        let risk_warning = external_chrome_popup_risk_host_unavailable_warning(popup_risks);
        return SubsystemHealth {
            status: "unavailable".to_owned(),
            detail: Some(format!(
                "tab_control_available=false reason=no_active_chrome_bridge_host host_count={} queued_count={} pending_count={} expected_extension_id={} endpoint={} repair_guidance={} {} {} {} {} {} install_guidance={}",
                host_count,
                queued_count,
                pending_count,
                EXTENSION_ID,
                chrome_debugger_health_endpoint(EXTENSION_ID),
                NO_ACTIVE_HOST_REPAIR_GUIDANCE,
                risk_warning,
                self_permission_warning,
                self_policy_shield.detail,
                profile_install_state.detail,
                layout_warning,
                INSTALL_GUIDANCE
            )),
            ..SubsystemHealth::default()
        };
    };

    let transport = host.transport.as_deref().unwrap_or("native_messaging");
    let extension_id = host.extension_id.as_deref().unwrap_or("not_seen_yet");
    let endpoint_extension_id = host.extension_id.as_deref().unwrap_or(EXTENSION_ID);
    let parent_window = host.parent_window.as_deref().unwrap_or("");
    let extension_version = host.extension_version.as_deref().unwrap_or("not_seen_yet");
    let extension_protocol_version = host
        .extension_protocol_version
        .map_or_else(|| "not_seen_yet".to_owned(), |value| value.to_string());
    let extension_build_id = host.extension_build_id.as_deref().unwrap_or("not_seen_yet");
    let extension_declared_build_sha256 = host
        .extension_declared_build_sha256
        .as_deref()
        .unwrap_or("not_seen_yet");
    let extension_service_worker_sha256 = host
        .extension_service_worker_sha256
        .as_deref()
        .unwrap_or("not_seen_yet");
    let extension_service_worker_sha256_status = host
        .extension_service_worker_sha256_status
        .as_deref()
        .unwrap_or("not_seen_yet");
    let extension_service_worker_sha256_source = host
        .extension_service_worker_sha256_source
        .as_deref()
        .unwrap_or("not_seen_yet");
    let extension_service_worker_byte_length = host
        .extension_service_worker_byte_length
        .map_or_else(|| "not_seen_yet".to_owned(), |value| value.to_string());
    let extension_service_worker_sha256_error = host
        .extension_service_worker_sha256_error
        .as_deref()
        .unwrap_or("none");
    let expected_service_worker_sha256 = host
        .expected_service_worker_sha256
        .as_deref()
        .unwrap_or("not_available");
    let expected_service_worker_path = host
        .expected_service_worker_path
        .as_deref()
        .map_or_else(|| "<none>".to_owned(), quote_detail_value);
    let extension_capabilities = format_capabilities(&host.extension_capabilities);
    let extension_user_agent = host.extension_user_agent.as_deref().unwrap_or("");
    let extension_debugger_api_available = host
        .extension_debugger_api_available
        .map_or_else(|| "not_seen_yet".to_owned(), |value| value.to_string());
    let popup_risk_suppression =
        popup_risk_suppression_summary(&host.extension_popup_risk_suppression);
    let extension_startup_readback = host.extension_startup_readback.as_ref().map_or_else(
        || "not_seen_yet".to_owned(),
        |value| quote_detail_value(&value.to_string()),
    );
    let risk_warning = external_chrome_popup_risk_warning(popup_risks);
    let stale_reasons = bridge_identity_stale_reasons(host);
    let extension_stale = !stale_reasons.is_empty();
    let extension_stale_reasons = if stale_reasons.is_empty() {
        "none".to_owned()
    } else {
        stale_reasons.join("|")
    };
    let tab_control_available = extension_id == EXTENSION_ID
        && host.last_disconnect_detail.is_none()
        && !extension_stale
        && !self_permission_blocking;
    let status = if tab_control_available {
        "ok"
    } else if extension_stale {
        "stale"
    } else if self_permission_blocking {
        "unsafe_profile"
    } else if host.last_disconnect_detail.is_some() {
        "unavailable"
    } else {
        "connecting"
    };
    let disconnect_detail = host.last_disconnect_detail.as_deref().unwrap_or("");
    let detach_reason = host.last_detach_reason.as_deref().unwrap_or("");

    SubsystemHealth {
        status: status.to_owned(),
        detail: Some(format!(
            "tab_control_available={} extension_stale={} extension_stale_reasons={} active_host_id={} host_count={} origin={} extension_id={} expected_extension_id={} extension_version={} extension_protocol_version={} extension_build_id={} expected_extension_build_id={} extension_declared_build_sha256={} expected_extension_declared_build_sha256={} extension_service_worker_sha256={} expected_extension_service_worker_sha256={} expected_extension_service_worker_path={} extension_service_worker_sha256_status={} extension_service_worker_sha256_source={} extension_service_worker_byte_length={} extension_service_worker_sha256_error={} extension_debugger_api_available={} expected_extension_debugger_api_available=false extension_capabilities={} required_extension_capabilities={} endpoint={} transport={} pid={} parent_window={} registered_unix_ms={} last_seen_unix_ms={} queued_count={} pending_count={} last_disconnect_detail={} last_detach_reason={} extension_user_agent={} bridge_popup_risk_suppression={} extension_startup_readback={} {} {} {} {} {} install_guidance={}",
            tab_control_available,
            extension_stale,
            extension_stale_reasons,
            host.host_id,
            host_count,
            host.origin,
            extension_id,
            EXTENSION_ID,
            extension_version,
            extension_protocol_version,
            extension_build_id,
            EXPECTED_EXTENSION_BUILD_ID,
            extension_declared_build_sha256,
            EXPECTED_EXTENSION_DECLARED_BUILD_SHA256,
            extension_service_worker_sha256,
            expected_service_worker_sha256,
            expected_service_worker_path,
            extension_service_worker_sha256_status,
            extension_service_worker_sha256_source,
            extension_service_worker_byte_length,
            extension_service_worker_sha256_error,
            extension_debugger_api_available,
            extension_capabilities,
            REQUIRED_DIRECT_HTTP_CAPABILITIES.join(","),
            chrome_debugger_health_endpoint(endpoint_extension_id),
            transport,
            host.pid,
            parent_window,
            host.registered_unix_ms,
            host.last_seen_unix_ms,
            queued_count,
            pending_count,
            disconnect_detail,
            detach_reason,
            extension_user_agent,
            popup_risk_suppression,
            extension_startup_readback,
            risk_warning,
            self_permission_warning,
            self_policy_shield.detail,
            profile_install_state.detail,
            layout_warning,
            INSTALL_GUIDANCE
        )),
        ..SubsystemHealth::default()
    }
}

fn chrome_debugger_health_endpoint(extension_id: &str) -> String {
    format!("chrome-extension://{extension_id}/chrome.tabs")
}

async fn send_attach_command(
    hwnd: i64,
    kind: &'static str,
    _payload: Value,
) -> Result<Value, ChromeDebuggerBridgeError> {
    let error = ChromeDebuggerBridgeError::normal_bridge_attach_disabled(hwnd, kind);
    tracing::warn!(
        code = error.code(),
        hwnd,
        command_kind = kind,
        detail = %error.detail(),
        "normal Chrome bridge refused attach-capable debugger command before queueing it to Chrome"
    );
    Err(error)
}

pub async fn click_node(
    hwnd: i64,
    foreground_title: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
    button: ChromeDebuggerMouseButton,
    click_count: i64,
) -> Result<ChromeDebuggerClickPoint, ChromeDebuggerBridgeError> {
    let result = send_attach_command(
        hwnd,
        "clickNode",
        json!({
            "hwnd": hwnd,
            "foregroundTitle": foreground_title,
            "targetIdHint": target_id_hint,
            "backendNodeId": backend_node_id,
            "button": button.as_str(),
            "clickCount": click_count.max(1),
        }),
    )
    .await?;
    serde_json::from_value::<ChromeDebuggerClickPoint>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger click response: {error}"
        ))
    })
}

pub async fn type_node(
    hwnd: i64,
    foreground_title: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
    text: &str,
) -> Result<ChromeDebuggerTypeResult, ChromeDebuggerBridgeError> {
    let result = send_attach_command(
        hwnd,
        "typeNode",
        json!({
            "hwnd": hwnd,
            "foregroundTitle": foreground_title,
            "targetIdHint": target_id_hint,
            "backendNodeId": backend_node_id,
            "text": text,
        }),
    )
    .await?;
    serde_json::from_value::<ChromeDebuggerTypeResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger type response: {error}"
        ))
    })
}

pub async fn node_value(
    hwnd: i64,
    foreground_title: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
) -> Result<ChromeDebuggerNodeValue, ChromeDebuggerBridgeError> {
    let result = send_attach_command(
        hwnd,
        "nodeValue",
        json!({
            "hwnd": hwnd,
            "foregroundTitle": foreground_title,
            "targetIdHint": target_id_hint,
            "backendNodeId": backend_node_id,
        }),
    )
    .await?;
    serde_json::from_value::<ChromeDebuggerNodeValue>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger node value response: {error}"
        ))
    })
}

pub async fn open_tab(
    hwnd: i64,
    url: &str,
    agent_session_id: Option<&str>,
    expected_chrome_window_id: Option<i64>,
    expected_window_bounds: Option<Rect>,
    expected_window_title: Option<&str>,
) -> Result<ChromeDebuggerOpenTabResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "openTab")?;
    let result = bridge()
        .send_command(
            "openTab",
            json!({
                "hwnd": hwnd,
                "url": url,
                "agentSessionId": agent_session_id,
                "expectedChromeWindowId": expected_chrome_window_id,
                "expectedWindowBounds": expected_window_bounds.map(|bounds| json!({
                    "x": bounds.x,
                    "y": bounds.y,
                    "w": bounds.w,
                    "h": bounds.h,
                })),
                "expectedWindowTitle": expected_window_title,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerOpenTabResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger open tab response: {error}"
        ))
    })
}

pub async fn list_tabs(
    hwnd: i64,
    expected_chrome_window_id: Option<i64>,
    expected_window_bounds: Option<Rect>,
    expected_window_title: Option<&str>,
) -> Result<ChromeDebuggerListTabsResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "listTabs")?;
    let result = bridge()
        .send_command(
            "listTabs",
            json!({
                "hwnd": hwnd,
                "expectedChromeWindowId": expected_chrome_window_id,
                "expectedWindowBounds": expected_window_bounds.map(|bounds| json!({
                    "x": bounds.x,
                    "y": bounds.y,
                    "w": bounds.w,
                    "h": bounds.h,
                })),
                "expectedWindowTitle": expected_window_title,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerListTabsResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger list tabs response: {error}"
        ))
    })
}

/// #1824: debugger-free synthetic key dispatch for a background bridge tab.
///
/// The real-mouse `cdpInput` lane and raw-CDP `Input.dispatchKeyEvent` both
/// require a CDP endpoint the normal authenticated Chrome profile does not
/// expose, which left `act verb=key` with no route at all on the transport that
/// can happily open a modal. This lane runs the key sequence through
/// `chrome.scripting` in the page. The events are untrusted by web-platform
/// rule. The public action response reports that through the versioned
/// `synapse.input_provenance.v1` record; this internal response deliberately
/// carries no parallel trust/default-action fragments that could drift.
pub async fn key_dispatch(
    hwnd: i64,
    target_id: &str,
    keys: &[String],
    wait_timeout_ms: u64,
    suppress_page_text: bool,
) -> Result<Value, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "keyDispatch")?;
    bridge()
        .send_command(
            "keyDispatch",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "keys": keys,
                "waitTimeoutMs": wait_timeout_ms,
                "suppressPageText": suppress_page_text,
            }),
        )
        .await
}

pub async fn close_tab(
    hwnd: i64,
    target_id: &str,
) -> Result<ChromeDebuggerCloseTabResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "closeTab")?;
    let result = bridge()
        .send_command(
            "closeTab",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerCloseTabResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger close tab response: {error}"
        ))
    })
}

pub async fn operator_panic_close_tab(
    hwnd: i64,
    target_id: &str,
    expected_disable_sequence: u64,
) -> Result<ChromeDebuggerCloseTabResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "operatorPanicCloseTab")?;
    let result = bridge()
        .send_command(
            "operatorPanicCloseTab",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "expectedDisableSequence": expected_disable_sequence,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerCloseTabResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger operator panic close tab response: {error}"
        ))
    })
}

pub async fn target_info(
    hwnd: i64,
    target_id: &str,
    expected_chrome_window_id: Option<i64>,
    expected_window_bounds: Option<Rect>,
    expected_window_title: Option<&str>,
) -> Result<ChromeDebuggerTargetInfo, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "targetInfoPageText")?;
    let result = bridge()
        .send_command(
            "targetInfoPageText",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "expectedChromeWindowId": expected_chrome_window_id,
                "expectedWindowBounds": expected_window_bounds.map(|bounds| json!({
                    "x": bounds.x,
                    "y": bounds.y,
                    "w": bounds.w,
                    "h": bounds.h,
                })),
                "expectedWindowTitle": expected_window_title,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerTargetInfo>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger target info response: {error}"
        ))
    })
}

pub async fn target_tab_state(
    hwnd: i64,
    target_id: &str,
    expected_chrome_window_id: Option<i64>,
    expected_window_bounds: Option<Rect>,
    expected_window_title: Option<&str>,
) -> Result<ChromeDebuggerTargetInfo, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "targetInfo")?;
    let result = bridge()
        .send_command(
            "targetInfo",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "expectedChromeWindowId": expected_chrome_window_id,
                "expectedWindowBounds": expected_window_bounds.map(|bounds| json!({
                    "x": bounds.x,
                    "y": bounds.y,
                    "w": bounds.w,
                    "h": bounds.h,
                })),
                "expectedWindowTitle": expected_window_title,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerTargetInfo>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger target tab state response: {error}"
        ))
    })
}

pub async fn frames(
    hwnd: i64,
    target_id: &str,
) -> Result<ChromeDebuggerFramesResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "frames")?;
    let result = bridge()
        .send_command(
            "frames",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerFramesResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger frames response: {error}"
        ))
    })
}

pub async fn downloads(
    hwnd: i64,
    params: Value,
) -> Result<ChromeDebuggerDownloadsResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "downloads")?;
    let mut payload = if params.is_object() {
        params
    } else {
        json!({})
    };
    payload["hwnd"] = json!(hwnd);
    // For waiting operations the extension blocks in-page up to waitTimeoutMs, so
    // the daemon command budget must outlive it; otherwise the fixed 30s default
    // fires first and a no-match wait reads as A11Y_CDP_EXTENSION_TIMEOUT (#1342).
    let command_timeout = downloads_command_timeout(&payload);
    let result = bridge()
        .send_command_with_timeout("downloads", payload, command_timeout)
        .await?;
    serde_json::from_value::<ChromeDebuggerDownloadsResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger downloads response: {error}"
        ))
    })
}

/// Daemon-side response budget for a `downloads` bridge command. Read-only
/// `list` keeps the default budget; `wait`/`save`/`move` extend it to the
/// caller's `waitTimeoutMs` plus a margin so the daemon never times out before
/// the extension's own bounded wait returns a clean no-match result (#1342).
fn downloads_command_timeout(payload: &Value) -> Duration {
    let operation = payload
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("list");
    if !matches!(operation, "wait" | "save" | "move") {
        return COMMAND_TIMEOUT;
    }
    // The MCP layer already clamps waitTimeoutMs to <= 300_000; default to the
    // bridge contract's 30_000 when absent. Add a 5s margin so the extension's
    // own timeout result wins the race over the daemon budget.
    let wait_ms = payload
        .get("waitTimeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(30_000);
    let budget = Duration::from_millis(wait_ms).saturating_add(Duration::from_secs(5));
    budget.max(COMMAND_TIMEOUT)
}

pub async fn type_active_element(
    hwnd: i64,
    target_id: &str,
    text: &str,
) -> Result<ChromeDebuggerTypeActiveElementResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "cdpInput")?;
    let result = bridge()
        .send_command(
            "cdpInput",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "action": "type_text",
                "activeElement": true,
                "text": text,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerTypeActiveElementResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger typeActiveElement response: {error}"
        ))
    })
}

/// Background-safe field REPLACE via the normal Chrome bridge (#1000/#717).
/// Resolves the target in-page by a strict CSS `selector` (exactly one
/// editable+visible match), a normal Chrome bridge `element_id`, or the current
/// `active_element`, replaces its value with the native prototype setter
/// (React-safe), and returns the raw before/after values for the daemon's
/// Source-of-Truth check. No foreground, no debugger attach; works on
/// inactive/occluded tabs UIA cannot perceive.
pub async fn set_field_value(
    hwnd: i64,
    target_id: &str,
    expected_chrome_window_id: Option<i64>,
    selector: Option<&str>,
    element_id: Option<&str>,
    active_element: bool,
    text: &str,
) -> Result<ChromeDebuggerSetFieldValueResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "setFieldValue")?;
    let result = bridge()
        .send_command(
            "setFieldValue",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "expectedChromeWindowId": expected_chrome_window_id,
                "selector": selector,
                "elementId": element_id,
                "activeElement": active_element,
                "text": text,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerSetFieldValueResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger setFieldValue response: {error}"
        ))
    })
}

pub async fn page_content(
    hwnd: i64,
    target_id: &str,
    max_bytes: usize,
) -> Result<ChromeDebuggerPageContentResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "pageContent")?;
    let result = bridge()
        .send_command(
            "pageContent",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "maxBytes": max_bytes,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerPageContentResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger pageContent response: {error}"
        ))
    })
}

pub async fn cookies(
    hwnd: i64,
    target_id: &str,
    mut params: Value,
) -> Result<Value, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "cookies")?;
    if !params.is_object() {
        params = json!({});
    }
    params["hwnd"] = json!(hwnd);
    params["targetIdHint"] = json!(target_id);
    bridge().send_command("cookies", params).await
}

pub async fn storage_state(
    hwnd: i64,
    target_id: &str,
    mut params: Value,
) -> Result<Value, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "storageState")?;
    if !params.is_object() {
        params = json!({});
    }
    params["hwnd"] = json!(hwnd);
    params["targetIdHint"] = json!(target_id);
    bridge().send_command("storageState", params).await
}

pub async fn set_content(
    hwnd: i64,
    target_id: &str,
    html: &str,
    wait_timeout_ms: u64,
    agent_session_id: Option<&str>,
) -> Result<ChromeDebuggerSetContentResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "setContent")?;
    let result = bridge()
        .send_command(
            "setContent",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "html": html,
                "waitTimeoutMs": wait_timeout_ms,
                "agentSessionId": agent_session_id,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerSetContentResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger setContent response: {error}"
        ))
    })
}

/// #1558: marker substituted for any redacted control value, in both the node
/// `value` field and the pre-rendered `snapshot` string.
const ARIA_SNAPSHOT_REDACTED_MARKER: &str = "[redacted]";

/// #1558: non-reversible short hash of a redacted control value, for equality
/// proofs without exposing the secret. Truncated SHA-256 hex is one-way: the
/// original value cannot be recovered from it. Prefixed to document the algorithm.
fn aria_value_hash(value: &str) -> String {
    let full = sha256_hex_lower(value.as_bytes());
    format!("sha256:{}", &full[..16.min(full.len())])
}

/// #1558: `true` when an `autocomplete` attribute marks a credential/secret value.
/// Handles space-separated token lists (e.g. "section-blue shipping cc-number")
/// and treats every `cc-*` token as sensitive per the research-backed policy.
fn aria_autocomplete_is_sensitive(autocomplete: &str) -> bool {
    autocomplete.split_whitespace().any(|token| {
        let token = token.trim().to_ascii_lowercase();
        matches!(
            token.as_str(),
            "current-password" | "new-password" | "one-time-code"
        ) || token.starts_with("cc-")
    })
}

/// #1558: case-insensitive secret-name predicate matching the same alternation
/// used by the browser-side builder. Separators are optional so `api_key`,
/// `api-key`, `apiKey`, and `apikey` all match. Substring (not word-bounded)
/// matching is intentional: over-redaction is acceptable, under-redaction is not.
fn aria_name_looks_secret(name: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "password",
        "passwd",
        "pwd",
        "secret",
        "token",
        "apikey",
        "api key",
        "api-key",
        "api_key",
        "auth",
        "bearer",
        "credential",
        "privatekey",
        "private key",
        "private-key",
        "private_key",
        "cvv",
        "cvc",
        "cardnumber",
        "card number",
        "card-number",
        "card_number",
        "ccnumber",
        "otp",
        "onetime",
        "one time",
        "one-time",
        "one_time",
        "passcode",
        "pin",
        "sessionid",
        "session id",
        "session-id",
        "session_id",
        "csrf",
    ];
    if name.is_empty() {
        return false;
    }
    let hay = name.to_ascii_lowercase();
    PATTERNS.iter().any(|needle| hay.contains(needle))
}

/// #1558: heuristic for a high-entropy / token-like value. Used only to fail
/// closed when field metadata is ambiguous/unknown: a long, whitespace-free run
/// mixing letters and digits is treated as a probable secret and redacted.
fn aria_value_is_high_entropy_token(value: &str) -> bool {
    let value = value.trim();
    let len = value.chars().count();
    if len < 16 {
        return false;
    }
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    let has_alpha = value.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = value.chars().any(|c| c.is_ascii_digit());
    has_alpha && has_digit
}

/// #1558: re-apply the redaction policy to a single node. Returns `true` when the
/// control's value must not be exposed. Ambiguous metadata fails closed.
fn aria_node_is_sensitive(node: &ChromeDebuggerAriaSnapshotNode) -> bool {
    if let Some(input_type) = node.input_type.as_deref() {
        let input_type = input_type.trim().to_ascii_lowercase();
        if input_type == "password" || input_type == "hidden" {
            return true;
        }
    }
    if let Some(autocomplete) = node.autocomplete.as_deref()
        && aria_autocomplete_is_sensitive(autocomplete)
    {
        return true;
    }
    if aria_name_looks_secret(&node.name) {
        return true;
    }
    // Ambiguous/unknown metadata + a token-like value fails closed (redact).
    let metadata_known = node.input_type.is_some() || node.autocomplete.is_some();
    if !metadata_known
        && let Some(value) = node.value.as_deref()
        && aria_value_is_high_entropy_token(value)
    {
        return true;
    }
    false
}

/// #1558: host-side, last-choke-point redaction pass over a deserialized aria
/// snapshot. Runs before the result is returned to MCP output/logs/audit and does
/// not trust the browser to have redacted. For every sensitive node it strips the
/// raw `value`, records `value_length` + a non-reversible `value_hash`, and sets
/// `redacted = true`; it then scrubs any surviving raw value substring out of the
/// pre-rendered `snapshot` string. Returns the total number of redacted nodes.
/// Never logs or returns any secret value.
fn redact_aria_snapshot(result: &mut ChromeDebuggerAriaSnapshotResult) -> usize {
    // Capture raw values we strip so we can scrub them from `snapshot` afterwards.
    let mut leaked_values: Vec<String> = Vec::new();
    for node in &mut result.nodes {
        let sensitive = node.redacted || aria_node_is_sensitive(node);
        if !sensitive {
            continue;
        }
        if let Some(raw) = node.value.take()
            && !raw.is_empty()
        {
            // Re-derive evidence from the value the host can actually see so a
            // stale/bypassed browser cannot suppress it.
            node.value_length = Some(raw.chars().count());
            node.value_hash = Some(aria_value_hash(&raw));
            leaked_values.push(raw);
        }
        // `node.value` is now None regardless of source; browser-supplied
        // value_length/value_hash (if any) are preserved when the host saw no raw.
        node.redacted = true;
    }
    // Scrub the pre-rendered snapshot string. Longest-first avoids partial-overlap
    // artifacts when one leaked value is a substring of another.
    if !leaked_values.is_empty() && !result.snapshot.is_empty() {
        leaked_values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        for raw in &leaked_values {
            if !raw.is_empty() {
                result.snapshot = result
                    .snapshot
                    .replace(raw.as_str(), ARIA_SNAPSHOT_REDACTED_MARKER);
            }
        }
    }
    result.nodes.iter().filter(|node| node.redacted).count()
}

pub async fn aria_snapshot(
    hwnd: i64,
    target_id: &str,
    root_element_id: Option<&str>,
    max_nodes: usize,
    max_depth: u32,
) -> Result<ChromeDebuggerAriaSnapshotResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "ariaSnapshot")?;
    let result = bridge()
        .send_command(
            "ariaSnapshot",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "rootElementId": root_element_id,
                "maxNodes": max_nodes,
                "maxDepth": max_depth,
            }),
        )
        .await?;
    let mut snapshot =
        serde_json::from_value::<ChromeDebuggerAriaSnapshotResult>(result).map_err(|error| {
            ChromeDebuggerBridgeError::protocol(format!(
                "decode Chrome debugger ariaSnapshot response: {error}"
            ))
        })?;
    // #1558: defense-in-depth. Redact sensitive form-control values on the host
    // before the snapshot can reach MCP output, logs, transcripts, or audit rows.
    let redacted_nodes = redact_aria_snapshot(&mut snapshot);
    if redacted_nodes > 0 {
        // Fail-loud, count only. Never log any value.
        tracing::warn!(
            code = "ARIA_SNAPSHOT_REDACTED",
            hwnd,
            target_id,
            redacted_nodes,
            node_count = snapshot.nodes.len(),
            "redacted sensitive form-control values from aria snapshot before return"
        );
    }
    Ok(snapshot)
}

pub async fn assert_poll(
    hwnd: i64,
    target_id: &str,
    locator: Value,
    limit: usize,
) -> Result<ChromeDebuggerAssertPollResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "assertPoll")?;
    let result = bridge()
        .send_command(
            "assertPoll",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "locator": locator,
                "limit": limit,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerAssertPollResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger assertPoll response: {error}"
        ))
    })
}

pub async fn locate_elements(
    hwnd: i64,
    target_id: &str,
    locator: Value,
    limit: usize,
) -> Result<ChromeDebuggerLocateElementsResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "locateElements")?;
    let result = bridge()
        .send_command(
            "locateElements",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "locator": locator,
                "limit": limit,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerLocateElementsResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger locateElements response: {error}"
        ))
    })
}

pub async fn inspect_element(
    hwnd: i64,
    target_id: &str,
    element_id: &str,
    max_html_bytes: usize,
) -> Result<ChromeDebuggerInspectElementResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "inspectElement")?;
    let result = bridge()
        .send_command(
            "inspectElement",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "elementId": element_id,
                "maxHtmlBytes": max_html_bytes,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerInspectElementResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger inspectElement response: {error}"
        ))
    })
}

pub async fn scroll_into_view(
    hwnd: i64,
    target_id: &str,
    element_id: &str,
) -> Result<ChromeDebuggerScrollIntoViewResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "scrollIntoView")?;
    let result = bridge()
        .send_command(
            "scrollIntoView",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "elementId": element_id,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerScrollIntoViewResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger scrollIntoView response: {error}"
        ))
    })
}

pub async fn wait_for_text(
    hwnd: i64,
    target_id: &str,
    state: &str,
    text: Option<&str>,
    timeout_ms: u64,
    polling_interval_ms: u64,
) -> Result<ChromeDebuggerWaitForTextResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "waitForText")?;
    let result = bridge()
        .send_command(
            "waitForText",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "state": state,
                "text": text,
                "timeoutMs": timeout_ms,
                "pollingIntervalMs": polling_interval_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerWaitForTextResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger waitForText response: {error}"
        ))
    })
}

pub async fn wait_for_load_state(
    hwnd: i64,
    target_id: &str,
    state: &str,
    timeout_ms: u64,
) -> Result<ChromeDebuggerWaitForLoadStateResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "waitForLoadState")?;
    let result = bridge()
        .send_command(
            "waitForLoadState",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "state": state,
                "timeoutMs": timeout_ms,
                "pollingIntervalMs": 100,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerWaitForLoadStateResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger waitForLoadState response: {error}"
        ))
    })
}

pub async fn wait_for_url(
    hwnd: i64,
    target_id: &str,
    url: &str,
    match_kind: &str,
    timeout_ms: u64,
    polling_interval_ms: u64,
) -> Result<ChromeDebuggerWaitForUrlResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "waitForUrl")?;
    let result = bridge()
        .send_command(
            "waitForUrl",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "url": url,
                "matchKind": match_kind,
                "timeoutMs": timeout_ms,
                "pollingIntervalMs": polling_interval_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerWaitForUrlResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger waitForUrl response: {error}"
        ))
    })
}

// The CDP network-wait predicates carry the full match surface (hwnd, target, url
// pattern, method, status, resource type, timeout and polling knobs) as distinct
// scalars captured at the call site; bundling them into a params struct would only
// relocate the same fields.
#[allow(clippy::too_many_arguments)]
pub async fn wait_for_request(
    hwnd: i64,
    target_id: &str,
    url: Option<&str>,
    match_kind: &str,
    method: Option<&str>,
    resource_type: Option<&str>,
    timeout_ms: u64,
    polling_interval_ms: u64,
) -> Result<ChromeDebuggerWaitForNetworkResult, ChromeDebuggerBridgeError> {
    wait_for_network(
        "waitForRequest",
        hwnd,
        target_id,
        url,
        match_kind,
        method,
        None,
        resource_type,
        timeout_ms,
        polling_interval_ms,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn wait_for_response(
    hwnd: i64,
    target_id: &str,
    url: Option<&str>,
    match_kind: &str,
    method: Option<&str>,
    status: Option<i64>,
    resource_type: Option<&str>,
    timeout_ms: u64,
    polling_interval_ms: u64,
) -> Result<ChromeDebuggerWaitForNetworkResult, ChromeDebuggerBridgeError> {
    wait_for_network(
        "waitForResponse",
        hwnd,
        target_id,
        url,
        match_kind,
        method,
        status,
        resource_type,
        timeout_ms,
        polling_interval_ms,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_network(
    kind: &'static str,
    hwnd: i64,
    target_id: &str,
    url: Option<&str>,
    match_kind: &str,
    method: Option<&str>,
    status: Option<i64>,
    resource_type: Option<&str>,
    timeout_ms: u64,
    polling_interval_ms: u64,
) -> Result<ChromeDebuggerWaitForNetworkResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, kind)?;
    let result = bridge()
        .send_command(
            kind,
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "url": url,
                "matchKind": match_kind,
                "method": method,
                "status": status,
                "resourceType": resource_type,
                "timeoutMs": timeout_ms,
                "pollingIntervalMs": polling_interval_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerWaitForNetworkResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger {kind} response: {error}"
        ))
    })
}

pub async fn wait_for_selector(
    hwnd: i64,
    target_id: &str,
    locator: Value,
    limit: usize,
    state: &str,
    timeout_ms: u64,
    polling_interval_ms: u64,
) -> Result<ChromeDebuggerWaitForSelectorResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "waitForSelector")?;
    let result = bridge()
        .send_command(
            "waitForSelector",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "locator": locator,
                "limit": limit,
                "state": state,
                "timeoutMs": timeout_ms,
                "pollingIntervalMs": polling_interval_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerWaitForSelectorResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger waitForSelector response: {error}"
        ))
    })
}

pub async fn clock(
    hwnd: i64,
    target_id: &str,
    operation: &str,
    time_unix_ms: Option<u64>,
    delta_ms: Option<u64>,
) -> Result<ChromeDebuggerClockResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "clock")?;
    let result = bridge()
        .send_command(
            "clock",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "operation": operation,
                "timeMs": time_unix_ms,
                "deltaMs": delta_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerClockResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger clock response: {error}"
        ))
    })
}

pub async fn page_events(
    hwnd: i64,
    target_id: &str,
    since_seq: Option<u64>,
    limit: usize,
    event_kind: Option<&str>,
    worker_type: Option<&str>,
) -> Result<ChromeDebuggerPageEventsResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "pageEvents")?;
    let result = bridge()
        .send_command(
            "pageEvents",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "sinceSeq": since_seq,
                "limit": limit,
                "eventKind": event_kind,
                "workerType": worker_type,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerPageEventsResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger pageEvents response: {error}"
        ))
    })
}

pub async fn navigate_tab(
    hwnd: i64,
    target_id: &str,
    action: &str,
    url: Option<&str>,
    wait_timeout_ms: u64,
    ignore_cache: bool,
    agent_session_id: Option<&str>,
) -> Result<ChromeDebuggerNavigateResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "navigateTab")?;
    let result = bridge()
        .send_command(
            "navigateTab",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "action": action,
                "url": url,
                "waitTimeoutMs": wait_timeout_ms,
                "ignoreCache": ignore_cache,
                "agentSessionId": agent_session_id,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerNavigateResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger navigate response: {error}"
        ))
    })
}

/// Background-intended tab activation (#1189): selects `target_id` as the
/// active tab in its own Chrome window via `chrome.tabs.update({active:true})`.
/// Callers that promise background-safe behavior must separately read the OS
/// foreground before/after and fail closed if Chrome becomes foreground.
/// External debugger/nativeMessaging risks must be suppressed by policy or the
/// bridge management fallback before this queues.
pub async fn activate_tab(
    hwnd: i64,
    target_id: &str,
    wait_timeout_ms: u64,
    expected_chrome_window_id: Option<i64>,
    expected_window_bounds: Option<Rect>,
    expected_window_title: Option<&str>,
) -> Result<ChromeDebuggerActivateTabResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "activateTab")?;
    let result = bridge()
        .send_command(
            "activateTab",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "waitTimeoutMs": wait_timeout_ms,
                "expectedChromeWindowId": expected_chrome_window_id,
                "expectedWindowBounds": expected_window_bounds.map(|bounds| json!({
                    "x": bounds.x,
                    "y": bounds.y,
                    "w": bounds.w,
                    "h": bounds.h,
                })),
                "expectedWindowTitle": expected_window_title,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerActivateTabResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger activateTab response: {error}"
        ))
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the MCP browser_evaluate parameters sent to the bridge, including the caller-configurable evaluate budget"
)]
/// Closes the extension mutation-admission gate immediately on receipt, ahead
/// of its normal command FIFO, durably records the transition, and returns its
/// monotonic disable generation. K2 must call this before draining daemon-side
/// delivered owners; cleanup/readback then serialize behind earlier handlers.
pub async fn operator_panic_disable()
-> Result<ChromeDebuggerExtensionOwnerReadback, ChromeDebuggerBridgeError> {
    let result = bridge()
        .send_command_with_timeout("operatorPanicDisable", json!({}), Duration::from_secs(30))
        .await?;
    serde_json::from_value(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger operatorPanicDisable response: {error}"
        ))
    })
}

/// Removes every extension-owned durable mutator for the exact disable
/// generation returned by [`operator_panic_disable`]. A newer disable event
/// makes the cleanup generation mismatch fail closed.
pub async fn operator_panic_cleanup(
    expected_disable_sequence: u64,
) -> Result<ChromeDebuggerExtensionCleanupReadback, ChromeDebuggerBridgeError> {
    let result = bridge()
        .send_command_with_timeout(
            "operatorPanicCleanup",
            json!({ "expectedDisableSequence": expected_disable_sequence }),
            Duration::from_secs(30),
        )
        .await?;
    serde_json::from_value(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger operatorPanicCleanup response: {error}"
        ))
    })
}

/// Independent extension owner readback, serialized after every earlier
/// extension command. It may reconcile caller-timeout owners only when the
/// same host remained active throughout; transport-loss owners deliberately
/// remain fail-closed until daemon teardown because a replacement host is not
/// authoritative for the old Chrome profile/worker.
pub async fn operator_panic_readback()
-> Result<ChromeDebuggerExtensionOwnerReadback, ChromeDebuggerBridgeError> {
    let host_before = active_bridge_host_lineage_snapshot()?;
    let result = bridge()
        .send_command_with_timeout("operatorPanicReadback", json!({}), Duration::from_secs(30))
        .await?;
    let host_after = active_bridge_host_lineage_snapshot()?;
    let readback: ChromeDebuggerExtensionOwnerReadback =
        serde_json::from_value(result).map_err(|error| {
            ChromeDebuggerBridgeError::protocol(format!(
                "decode Chrome debugger operatorPanicReadback response: {error}"
            ))
        })?;
    if let ((Some(before), before_sequence), (Some(after), after_sequence)) =
        (host_before, host_after)
        && before == after
        && before_sequence == after_sequence
    {
        resolve_abandoned_delivered_mutations_after_extension_empty_readback(&readback, &before)?;
    }
    Ok(readback)
}

/// Reopens extension mutation admission only when no newer disable generation
/// superseded `expected_disable_sequence` and independent owner readback is
/// empty. The extension never recreates prior durable owners on reset.
pub async fn operator_panic_enable_if_unchanged(
    expected_disable_sequence: u64,
) -> Result<ChromeDebuggerExtensionEnableReadback, ChromeDebuggerBridgeError> {
    let result = bridge()
        .send_command_with_timeout(
            "operatorPanicEnable",
            json!({ "expectedDisableSequence": expected_disable_sequence }),
            Duration::from_secs(30),
        )
        .await?;
    serde_json::from_value(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger operatorPanicEnable response: {error}"
        ))
    })
}

pub struct ChromeDebuggerDomActionRequest<'a> {
    pub hwnd: i64,
    pub target_id: &'a str,
    pub action: &'a str,
    pub selector: Option<&'a str>,
    pub element_id: Option<&'a str>,
    pub role: Option<&'a str>,
    pub name: Option<&'a str>,
    pub value: Option<&'a str>,
    pub option: Option<&'a str>,
    pub option_label: Option<&'a str>,
    pub option_index: Option<i32>,
    pub options: Option<&'a Value>,
    pub event_type: Option<&'a str>,
    pub event_init: Option<&'a Value>,
    pub clicks: Option<u8>,
    pub button: Option<&'a str>,
    pub modifiers: Option<&'a Value>,
    pub position_x: Option<i32>,
    pub position_y: Option<i32>,
    pub wait_timeout_ms: u64,
    pub auto_wait: bool,
    pub auto_wait_timeout_ms: u32,
    /// #1821 Playwright `force` parity: dispatch on the resolved node even when
    /// an actionability predicate is unmet, recording the bypassed snapshot.
    pub force: bool,
    pub suppress_page_text: bool,
}

pub async fn dom_action(
    request: ChromeDebuggerDomActionRequest<'_>,
) -> Result<Value, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(request.hwnd, "domAction")?;
    bridge()
        .send_command(
            "domAction",
            json!({
                "hwnd": request.hwnd,
                "targetIdHint": request.target_id,
                "action": request.action,
                "selector": request.selector,
                "elementId": request.element_id,
                "role": request.role,
                "name": request.name,
                "value": request.value,
                "option": request.option,
                "optionLabel": request.option_label,
                "optionIndex": request.option_index,
                "options": request.options,
                "eventType": request.event_type,
                "eventInit": request.event_init,
                "clicks": request.clicks,
                "button": request.button,
                "modifiers": request.modifiers,
                "positionX": request.position_x,
                "positionY": request.position_y,
                "waitTimeoutMs": request.wait_timeout_ms,
                "autoWait": request.auto_wait,
                "autoWaitTimeoutMs": request.auto_wait_timeout_ms,
                "force": request.force,
                "suppressPageText": request.suppress_page_text,
            }),
        )
        .await
}

pub struct ChromeDebuggerCoordinateClickRequest<'a> {
    pub hwnd: i64,
    pub target_id: &'a str,
    pub x: i32,
    pub y: i32,
    pub coordinate_space: &'a str,
    pub clicks: u8,
    pub button: Option<&'a str>,
    pub modifiers: Option<&'a Value>,
    pub wait_timeout_ms: u64,
    pub suppress_page_text: bool,
}

pub async fn coordinate_click(
    request: ChromeDebuggerCoordinateClickRequest<'_>,
) -> Result<Value, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(request.hwnd, "coordinateClick")?;
    bridge()
        .send_command(
            "coordinateClick",
            json!({
                "hwnd": request.hwnd,
                "targetIdHint": request.target_id,
                "x": request.x,
                "y": request.y,
                "coordinateSpace": request.coordinate_space,
                "clicks": request.clicks,
                "button": request.button,
                "modifiers": request.modifiers,
                "waitTimeoutMs": request.wait_timeout_ms,
                "suppressPageText": request.suppress_page_text,
            }),
        )
        .await
}

pub fn validate_reload_wait_timeout(value: Option<u64>) -> Result<u64, ChromeDebuggerBridgeError> {
    let value = value.unwrap_or(DEFAULT_RELOAD_WAIT_TIMEOUT_MS);
    if value == 0 || value > MAX_RELOAD_WAIT_TIMEOUT_MS {
        return Err(ChromeDebuggerBridgeError::params_invalid(format!(
            "cdp_bridge_reload wait_timeout_ms must be 1..={MAX_RELOAD_WAIT_TIMEOUT_MS}, got {value}; remediation=use a positive wait_timeout_ms at or below {MAX_RELOAD_WAIT_TIMEOUT_MS}; source_of_truth=local cdp_bridge_reload parameter validation before bridge reload command"
        )));
    }
    Ok(value)
}

fn validate_maintenance_reconnect_pause_ms(
    value: Option<u64>,
) -> Result<u64, ChromeDebuggerBridgeError> {
    let value = value.unwrap_or(DEFAULT_MAINTENANCE_RECONNECT_PAUSE_MS);
    if !(MIN_MAINTENANCE_RECONNECT_PAUSE_MS..=MAX_MAINTENANCE_RECONNECT_PAUSE_MS).contains(&value) {
        return Err(ChromeDebuggerBridgeError::params_invalid(format!(
            "chrome bridge maintenance reconnect pause_ms must be {MIN_MAINTENANCE_RECONNECT_PAUSE_MS}..={MAX_MAINTENANCE_RECONNECT_PAUSE_MS}, got {value}; remediation=use a bounded maintenance pause long enough for setup handoff but not an unbounded reconnect blackout"
        )));
    }
    Ok(value)
}

fn validate_maintenance_reconnect_resume_probe_after_ms(
    value: Option<u64>,
    pause_ms: u64,
) -> Result<u64, ChromeDebuggerBridgeError> {
    let value = value.unwrap_or(DEFAULT_MAINTENANCE_RECONNECT_RESUME_PROBE_AFTER_MS);
    if !(MIN_MAINTENANCE_RECONNECT_PAUSE_MS..=pause_ms).contains(&value) {
        return Err(ChromeDebuggerBridgeError::params_invalid(format!(
            "chrome bridge maintenance reconnect resume_probe_after_ms must be {MIN_MAINTENANCE_RECONNECT_PAUSE_MS}..={pause_ms}, got {value}; remediation=set the resume probe after the shutdown/socket-drain critical section but before the bounded pause expires"
        )));
    }
    Ok(value)
}

fn validate_maintenance_reconnect_pause_reason(
    value: &str,
) -> Result<String, ChromeDebuggerBridgeError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 {
        return Err(ChromeDebuggerBridgeError::params_invalid(format!(
            "chrome bridge maintenance reconnect pause reason must be 1..=256 chars, got length={}; remediation=send a short operator-visible setup reason for the reconnect suppression window",
            value.len()
        )));
    }
    Ok(value.to_owned())
}

pub async fn reload_bridge(
    wait_timeout_ms: u64,
) -> Result<ChromeBridgeReloadResult, ChromeDebuggerBridgeError> {
    bridge().reload_in_background(wait_timeout_ms).await
}

pub async fn wait_for_active_bridge_host(
    wait_timeout_ms: u64,
) -> Result<ChromeBridgeHostSnapshot, ChromeDebuggerBridgeError> {
    bridge()
        .wait_for_active_host(Duration::from_millis(wait_timeout_ms))
        .await
}

pub async fn maintenance_pause_reconnect(
    reason: &str,
    pause_ms: Option<u64>,
    resume_probe_after_ms: Option<u64>,
) -> Result<ChromeBridgeMaintenancePauseAck, ChromeDebuggerBridgeError> {
    let pause_ms = validate_maintenance_reconnect_pause_ms(pause_ms)?;
    let resume_probe_after_ms =
        validate_maintenance_reconnect_resume_probe_after_ms(resume_probe_after_ms, pause_ms)?;
    let reason = validate_maintenance_reconnect_pause_reason(reason)?;
    bridge()
        .maintenance_pause_reconnect(reason, pause_ms, resume_probe_after_ms)
        .await
}

pub fn is_direct_http_extension_bridge_request(headers: &HeaderMap, uri: &Uri) -> bool {
    let path = uri.path();
    if !path.starts_with("/chrome-debugger/native/") {
        return false;
    }
    if !matches!(
        path,
        "/chrome-debugger/native/next"
            | "/chrome-debugger/native/message"
            | "/chrome-debugger/native/ws"
    ) {
        return false;
    }
    if path == "/chrome-debugger/native/ws"
        && uri
            .query()
            .and_then(bridge_token_from_query)
            .is_some_and(|token| bridge().direct_http_bridge_token_matches(token))
    {
        return true;
    }
    bridge_token_from_headers(headers)
        .is_some_and(|token| bridge().direct_http_bridge_token_matches(token))
}

pub fn is_direct_http_extension_bridge_register_or_probe_request(
    headers: &HeaderMap,
    uri: &Uri,
) -> bool {
    if !matches!(
        uri.path(),
        "/chrome-debugger/native/register" | "/chrome-debugger/native/reconnect-probe"
    ) {
        return false;
    }
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .is_none_or(|origin| origin == EXTENSION_ORIGIN)
}

pub fn is_direct_http_extension_bridge_cors_preflight_request(
    method: &Method,
    headers: &HeaderMap,
    uri: &Uri,
) -> bool {
    if *method != Method::OPTIONS {
        return false;
    }
    if !matches!(
        uri.path(),
        "/chrome-debugger/native/register"
            | "/chrome-debugger/native/reconnect-probe"
            | "/chrome-debugger/native/message"
            | "/chrome-debugger/native/next"
    ) {
        return false;
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    if origin.is_some_and(|origin| origin != EXTENSION_ORIGIN) {
        return false;
    }
    headers
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .is_some_and(|method| matches!(method, "GET" | "POST" | "OPTIONS"))
}

pub fn direct_http_bridge_cors_preflight_response() -> Response {
    with_direct_http_bridge_cors_headers(StatusCode::NO_CONTENT.into_response())
}

fn with_direct_http_bridge_cors_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static(EXTENSION_ORIGIN),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(DIRECT_HTTP_BRIDGE_CORS_ALLOW_METHODS),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(DIRECT_HTTP_BRIDGE_CORS_ALLOW_HEADERS),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    headers.insert(
        header::VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    response
}

fn direct_http_bridge_token_header_matches_host(headers: &HeaderMap, host_id: &str) -> bool {
    bridge_token_from_headers(headers)
        .is_some_and(|token| bridge().direct_http_bridge_token_matches_host(host_id, token))
}

fn bridge_token_from_headers(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(BRIDGE_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

pub async fn http_register(Json(request): Json<NativeRegisterRequest>) -> Response {
    with_direct_http_bridge_cors_headers(match bridge().register(request) {
        Ok(response) => Json(response).into_response(),
        Err(detail) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "code": error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE,
                "detail": detail,
            })),
        )
            .into_response(),
    })
}

pub async fn http_message(
    headers: HeaderMap,
    request: Result<Json<NativeMessageRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(rejection) => {
            let status = rejection.status();
            let detail = format!(
                "Chrome bridge HTTP event ingress rejected before dispatch; status={} limit_bytes={} extractor_detail={}; remediation=send only bounded non-command events over this route; command terminals must use the authenticated WebSocket",
                status.as_u16(),
                NATIVE_EVENT_HTTP_BODY_LIMIT_BYTES,
                rejection.body_text(),
            );
            tracing::error!(
                code = error_codes::CHROME_BRIDGE_MESSAGE_BODY_EXCEEDS_LIMIT,
                http_status = status.as_u16(),
                limit_bytes = NATIVE_EVENT_HTTP_BODY_LIMIT_BYTES,
                detail = %detail,
                "Chrome bridge HTTP event ingress rejected"
            );
            return with_direct_http_bridge_cors_headers(
                (
                    status,
                    Json(json!({
                        "ok": false,
                        "code": error_codes::CHROME_BRIDGE_MESSAGE_BODY_EXCEEDS_LIMIT,
                        "detail": detail,
                        "limit_bytes": NATIVE_EVENT_HTTP_BODY_LIMIT_BYTES,
                    })),
                )
                    .into_response(),
            );
        }
    };
    if bridge_token_from_headers(&headers).is_some()
        && !direct_http_bridge_token_header_matches_host(&headers, &request.host_id)
    {
        return with_direct_http_bridge_cors_headers(direct_http_bridge_token_rejected(
            &request.host_id,
        ));
    }
    with_direct_http_bridge_cors_headers(match bridge().post_message(request) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(detail) => {
            let code = if detail.starts_with(error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR) {
                error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR
            } else {
                error_codes::A11Y_CDP_ATTACH_FAILED
            };
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "code": code,
                    "detail": detail,
                })),
            )
                .into_response()
        }
    })
}

pub async fn http_maintenance_pause(
    Json(request): Json<NativeMaintenancePauseRequest>,
) -> Response {
    with_direct_http_bridge_cors_headers(
        match maintenance_pause_reconnect(
            &request.reason,
            request.pause_ms,
            request.resume_probe_after_ms,
        )
        .await
        {
            Ok(pause) => Json(json!({
                "ok": true,
                "pause": pause,
            }))
            .into_response(),
            Err(error) => {
                let status = if error.code() == error_codes::TOOL_PARAMS_INVALID {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                (
                    status,
                    Json(json!({
                        "ok": false,
                        "code": error.code(),
                        "detail": error.detail(),
                    })),
                )
                    .into_response()
            }
        },
    )
}

pub async fn http_reconnect_probe() -> Response {
    with_direct_http_bridge_cors_headers(
        Json(NativeReconnectProbeResponse {
            ok: true,
            daemon_pid: std::process::id(),
            daemon_instance_id: daemon_instance_id().to_owned(),
            daemon_now_unix_ms: now_unix_ms(),
            bridge_protocol_version: BRIDGE_PROTOCOL_VERSION,
            expected_extension_id: EXTENSION_ID.to_owned(),
            expected_extension_build_id: EXPECTED_EXTENSION_BUILD_ID.to_owned(),
        })
        .into_response(),
    )
}

pub async fn http_next(headers: HeaderMap, Query(query): Query<NativeNextQuery>) -> Response {
    if bridge_token_from_headers(&headers).is_some()
        && !direct_http_bridge_token_header_matches_host(&headers, &query.host_id)
    {
        return with_direct_http_bridge_cors_headers(direct_http_bridge_token_rejected(
            &query.host_id,
        ));
    }
    let timeout_ms = query
        .timeout_ms
        .unwrap_or_else(|| u64::try_from(NATIVE_POLL_TIMEOUT.as_millis()).unwrap_or(15_000))
        .min(30_000);
    with_direct_http_bridge_cors_headers(
        match bridge()
            .next_command(&query.host_id, Duration::from_millis(timeout_ms))
            .await
        {
            Ok(command) => Json(NativeNextResponse { ok: true, command }).into_response(),
            Err(detail) => (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "code": error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE,
                    "detail": detail,
                })),
            )
                .into_response(),
        },
    )
}

fn direct_http_bridge_token_rejected(host_id: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "ok": false,
            "code": error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE,
            "detail": format!("direct Chrome debugger bridge token did not match host_id {host_id:?}"),
        })),
    )
        .into_response()
}

pub async fn http_ws(Query(query): Query<NativeWsQuery>, ws: WebSocketUpgrade) -> Response {
    if !bridge().direct_http_bridge_token_matches_host(&query.host_id, &query.bridge_token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "ok": false,
                "code": error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE,
                "detail": "direct Chrome debugger bridge WebSocket token did not match registered host",
            })),
        )
            .into_response();
    }
    let host_id = query.host_id;
    ws.max_message_size(NATIVE_MESSAGE_HTTP_BODY_LIMIT_BYTES)
        .max_frame_size(NATIVE_MESSAGE_HTTP_BODY_LIMIT_BYTES)
        .on_upgrade(move |socket| direct_http_ws_loop(socket, host_id))
}

async fn direct_http_ws_loop(socket: WebSocket, host_id: String) {
    tracing::info!(
        code = "CHROME_DEBUGGER_DIRECT_HTTP_WS_CONNECTED",
        host_id = %host_id,
        "Chrome debugger direct HTTP WebSocket connected"
    );
    let (mut sender, mut receiver) = socket.split();
    let mut disconnect_detail = "client closed direct HTTP WebSocket".to_owned();
    loop {
        tokio::select! {
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Text(raw))) => {
                        if !bridge().touch_host(&host_id) {
                            disconnect_detail = "registered direct HTTP host disappeared while processing WebSocket text".to_owned();
                            break;
                        }
                        let message_type = serde_json::from_str::<Value>(raw.as_str())
                            .ok()
                            .and_then(|value| value.get("type").and_then(Value::as_str).map(ToOwned::to_owned));
                        if message_type.as_deref() == Some("keepalive") {
                            continue;
                        }
                        let acknowledgement = bridge().accept_command_terminal(&host_id, raw.as_str());
                        let terminal_rejected = acknowledgement.get("ok").and_then(Value::as_bool) == Some(false);
                        if let Err(error) = sender.send(Message::Text(acknowledgement.to_string().into())).await {
                            disconnect_detail = format!("failed to send command-terminal acknowledgement: {error}");
                            break;
                        }
                        if terminal_rejected {
                            disconnect_detail = "daemon rejected contradictory command terminal; extension must remain fail-closed".to_owned();
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(payload))) => {
                        disconnect_detail = format!(
                            "direct HTTP WebSocket binary client payload is forbidden; bytes={}",
                            payload.len()
                        );
                        let rejection = json!({
                            "type": "command_terminal_nack",
                            "ok": false,
                            "code": error_codes::CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR,
                            "command_id": "",
                            "detail": disconnect_detail,
                        });
                        let _ = sender.send(Message::Text(rejection.to_string().into())).await;
                        break;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        if !bridge().touch_host(&host_id) {
                            disconnect_detail = "registered direct HTTP host disappeared while processing WebSocket pong".to_owned();
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if !bridge().touch_host(&host_id) {
                            disconnect_detail = "registered direct HTTP host disappeared while processing WebSocket ping".to_owned();
                            break;
                        }
                        if let Err(error) = sender.send(Message::Pong(payload)).await {
                            disconnect_detail = format!("failed to send direct HTTP WebSocket pong: {error}");
                            break;
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        disconnect_detail = format!("client closed direct HTTP WebSocket frame={frame:?}");
                        break;
                    }
                    Some(Err(error)) => {
                        disconnect_detail = format!("direct HTTP WebSocket receive failed: {error}");
                        break;
                    }
                    None => {
                        disconnect_detail = "direct HTTP WebSocket receive stream ended".to_owned();
                        break;
                    }
                }
            }
            command_result = bridge().next_command(&host_id, DIRECT_WS_COMMAND_WAIT) => {
                let payload = match command_result {
                    Ok(command) => json!({
                        "ok": true,
                        "command": command,
                    }),
                    Err(detail) => {
                        disconnect_detail = format!("direct HTTP WebSocket command wait failed: {detail}");
                        json!({
                            "ok": false,
                            "code": error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE,
                            "detail": disconnect_detail,
                        })
                    }
                };
                if let Err(error) = sender.send(Message::Text(payload.to_string().into())).await {
                    disconnect_detail = format!("failed to send direct HTTP WebSocket payload: {error}");
                    break;
                }
                if payload.get("ok").and_then(Value::as_bool) == Some(false) {
                    break;
                }
            }
        }
    }
    bridge().disconnect_direct_http_host(&host_id, &disconnect_detail);
}

pub async fn run_native_host(
    bind: &str,
    invocation: NativeHostInvocation,
) -> anyhow::Result<ExitCode> {
    let token = load_token_value().context("load Synapse HTTP bearer token")?;
    let base_url = http_base_url(bind);
    let client = reqwest::Client::new();
    let pid = std::process::id();
    let registered = register_native_host(
        &client,
        &base_url,
        &token,
        &invocation,
        pid,
        "native_host_start",
    )
    .await?;
    tracing::info!(
        code = "CHROME_DEBUGGER_NATIVE_HOST_STARTED",
        host_id = %registered.host_id,
        origin = %invocation.origin,
        pid,
        "Chrome debugger native host bridge started"
    );

    let host_id = Arc::new(RwLock::new(registered.host_id));
    let reader_client = client.clone();
    let reader_token = token.clone();
    let reader_base_url = base_url.clone();
    let reader_invocation = invocation.clone();
    let reader_host_id = Arc::clone(&host_id);
    let mut reader_task = tokio::spawn(async move {
        read_native_messages(
            reader_client,
            reader_base_url,
            reader_token,
            reader_invocation,
            pid,
            reader_host_id,
        )
        .await
    });
    let mut poll_task = tokio::spawn(async move {
        poll_commands_to_chrome(client, base_url, token, invocation, pid, host_id).await
    });

    tokio::select! {
        reader_result = &mut reader_task => {
            poll_task.abort();
            match reader_result {
                Ok(Ok(())) => {
                    tracing::info!(
                        code = "CHROME_DEBUGGER_NATIVE_HOST_EXITED",
                        pid,
                        "Chrome debugger native host exiting after stdin EOF"
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Ok(Err(error)) => Err(error),
                Err(error) => Err(anyhow::anyhow!(
                    "Chrome debugger native host reader task failed: {error}"
                )),
            }
        }
        poll_result = &mut poll_task => {
            reader_task.abort();
            match poll_result {
                Ok(result) => result,
                Err(error) => Err(anyhow::anyhow!(
                    "Chrome debugger native host poll task failed: {error}"
                )),
            }
        }
    }
}

async fn register_native_host(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    invocation: &NativeHostInvocation,
    pid: u32,
    reason: &'static str,
) -> anyhow::Result<NativeRegisterResponse> {
    let register = NativeRegisterRequest {
        origin: invocation.origin.clone(),
        pid,
        parent_window: invocation.parent_window.clone(),
        bridge_protocol_version: BRIDGE_PROTOCOL_VERSION,
        transport: Some("native_messaging".to_owned()),
    };
    let response = client
        .post(format!("{base_url}/chrome-debugger/native/register"))
        .bearer_auth(token)
        .json(&register)
        .send()
        .await
        .context("register Chrome debugger native host with Synapse daemon")?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "Chrome debugger native host register failed status={status} detail={detail}"
        );
    }
    let registered = response
        .json::<NativeRegisterResponse>()
        .await
        .context("decode Chrome debugger native register response")?;
    tracing::info!(
        code = "CHROME_DEBUGGER_NATIVE_HOST_REGISTERED_WITH_DAEMON",
        host_id = %registered.host_id,
        origin = %invocation.origin,
        pid,
        reason,
        "Chrome debugger native host registered with daemon"
    );
    Ok(registered)
}

/// The native host authenticates to the daemon, so it must resolve the bearer
/// token by exactly the daemon's rule. This used to be a second, independent
/// copy of the resolution logic; #2099 changed the precedence in `http::auth`
/// and a divergent copy here would authenticate against a token the daemon is
/// not using. There is now one implementation.
fn load_token_value() -> anyhow::Result<String> {
    crate::bearer_token::load_token_value()
}

async fn read_native_messages(
    client: reqwest::Client,
    base_url: String,
    token: String,
    invocation: NativeHostInvocation,
    pid: u32,
    host_id: Arc<RwLock<String>>,
) -> anyhow::Result<()> {
    let mut stdin = tokio::io::stdin();
    let registration = NativeHostRegistrationContext {
        client: &client,
        base_url: &base_url,
        token: &token,
        invocation: &invocation,
        pid,
    };
    loop {
        let Some(message) = read_native_frame(&mut stdin).await? else {
            let current_host_id = host_id.read().await.clone();
            let _ = post_native_message(
                &client,
                &base_url,
                &token,
                &current_host_id,
                json!({
                    "type": "event",
                    "event": "nativePortDisconnected",
                    "detail": "stdin EOF from Chrome native messaging port",
                }),
            )
            .await;
            return Ok(());
        };
        let mut current_host_id = host_id.read().await.clone();
        match post_native_message(
            &client,
            &base_url,
            &token,
            &current_host_id,
            message.clone(),
        )
        .await
        {
            Ok(()) => {}
            Err(error) if is_unknown_native_host_error(&error) => {
                reregister_native_host_until_available(
                    &registration,
                    &host_id,
                    &current_host_id,
                    "message_unknown_host_id",
                )
                .await?;
                current_host_id = host_id.read().await.clone();
                post_native_message(&client, &base_url, &token, &current_host_id, message).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn poll_commands_to_chrome(
    client: reqwest::Client,
    base_url: String,
    token: String,
    invocation: NativeHostInvocation,
    pid: u32,
    host_id: Arc<RwLock<String>>,
) -> anyhow::Result<ExitCode> {
    let mut stdout = tokio::io::stdout();
    let registration = NativeHostRegistrationContext {
        client: &client,
        base_url: &base_url,
        token: &token,
        invocation: &invocation,
        pid,
    };
    loop {
        let current_host_id = host_id.read().await.clone();
        let response = match client
            .get(format!(
                "{base_url}/chrome-debugger/native/next?host_id={current_host_id}&timeout_ms=15000"
            ))
            .bearer_auth(&token)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) if is_transient_daemon_transport_error(&error) => {
                tracing::warn!(
                    code = "CHROME_DEBUGGER_NATIVE_DAEMON_UNREACHABLE",
                    host_id = %current_host_id,
                    error = %error,
                    "Chrome debugger native host waiting for Synapse daemon transport"
                );
                tokio::time::sleep(NATIVE_DAEMON_RECONNECT_DELAY).await;
                continue;
            }
            Err(error) => return Err(error).context("poll Chrome debugger daemon command queue"),
        };
        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            if is_unknown_native_host_detail(&detail) {
                reregister_native_host_until_available(
                    &registration,
                    &host_id,
                    &current_host_id,
                    "poll_unknown_host_id",
                )
                .await?;
                continue;
            }
            anyhow::bail!("Chrome debugger native poll failed status={status} detail={detail}");
        }
        let next = response
            .json::<NativeNextResponse>()
            .await
            .context("decode Chrome debugger native poll response")?;
        if let Some(command) = next.command {
            write_native_frame(&mut stdout, &serde_json::to_value(command)?).await?;
        }
    }
}

struct NativeHostRegistrationContext<'a> {
    client: &'a reqwest::Client,
    base_url: &'a str,
    token: &'a str,
    invocation: &'a NativeHostInvocation,
    pid: u32,
}

async fn reregister_native_host_until_available(
    registration: &NativeHostRegistrationContext<'_>,
    host_id: &Arc<RwLock<String>>,
    observed_host_id: &str,
    reason: &'static str,
) -> anyhow::Result<()> {
    if host_id.read().await.as_str() != observed_host_id {
        return Ok(());
    }
    loop {
        match register_native_host(
            registration.client,
            registration.base_url,
            registration.token,
            registration.invocation,
            registration.pid,
            reason,
        )
        .await
        {
            Ok(registered) => {
                tracing::warn!(
                    code = "CHROME_DEBUGGER_NATIVE_HOST_REREGISTERED",
                    old_host_id = %observed_host_id,
                    new_host_id = %registered.host_id,
                    reason,
                    "Chrome debugger native host re-registered after daemon bridge state changed"
                );
                *host_id.write().await = registered.host_id;
                return Ok(());
            }
            Err(error) if is_transient_daemon_register_error(&error) => {
                tracing::warn!(
                    code = "CHROME_DEBUGGER_NATIVE_HOST_REREGISTER_RETRY",
                    old_host_id = %observed_host_id,
                    reason,
                    error = %format!("{error:#}"),
                    "Chrome debugger native host waiting to re-register with Synapse daemon"
                );
                tokio::time::sleep(NATIVE_DAEMON_RECONNECT_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_unknown_native_host_error(error: &anyhow::Error) -> bool {
    is_unknown_native_host_detail(&format!("{error:#}"))
}

fn is_unknown_native_host_detail(detail: &str) -> bool {
    detail.contains(UNKNOWN_NATIVE_HOST_ID_FRAGMENT)
}

fn is_transient_daemon_transport_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout() || error.is_request()
}

fn is_transient_daemon_register_error(error: &anyhow::Error) -> bool {
    let detail = format!("{error:#}").to_ascii_lowercase();
    detail.contains("error sending request")
        || detail.contains("connection refused")
        || detail.contains("connection reset")
        || detail.contains("timed out")
        || detail.contains("operation timed out")
}

async fn post_native_message(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    host_id: &str,
    message: Value,
) -> anyhow::Result<()> {
    let response = client
        .post(format!("{base_url}/chrome-debugger/native/message"))
        .bearer_auth(token)
        .json(&NativeMessageRequest {
            host_id: host_id.to_owned(),
            message,
        })
        .send()
        .await
        .context("post Chrome debugger extension message to Synapse daemon")?;
    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        anyhow::bail!("Chrome debugger message post failed status={status} detail={detail}");
    }
}

async fn read_native_frame<R>(reader: &mut R) -> anyhow::Result<Option<Value>>
where
    R: AsyncRead + Unpin,
{
    let mut len = [0_u8; 4];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("read Chrome native message length"),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_NATIVE_MESSAGE_FROM_CHROME {
        anyhow::bail!(
            "Chrome native message length {len} exceeds max {MAX_NATIVE_MESSAGE_FROM_CHROME}"
        );
    }
    let mut body = vec![0_u8; len];
    reader
        .read_exact(&mut body)
        .await
        .context("read Chrome native message body")?;
    let message = serde_json::from_slice(&body).context("decode Chrome native JSON message")?;
    Ok(Some(message))
}

async fn write_native_frame<W>(writer: &mut W, value: &Value) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(value).context("encode Chrome native JSON message")?;
    if body.len() > MAX_NATIVE_MESSAGE_TO_CHROME {
        anyhow::bail!(
            "Chrome native response length {} exceeds max {MAX_NATIVE_MESSAGE_TO_CHROME}",
            body.len()
        );
    }
    let len = u32::try_from(body.len()).context("Chrome native response length overflow")?;
    writer
        .write_all(&len.to_le_bytes())
        .await
        .context("write Chrome native message length")?;
    writer
        .write_all(&body)
        .await
        .context("write Chrome native message body")?;
    writer.flush().await.context("flush Chrome native stdout")?;
    Ok(())
}

fn http_base_url(bind: &str) -> String {
    if bind.starts_with("http://") || bind.starts_with("https://") {
        bind.trim_end_matches('/').to_owned()
    } else {
        format!("http://{}", bind.trim_end_matches('/'))
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn daemon_instance_id() -> &'static str {
    static INSTANCE_ID: OnceLock<String> = OnceLock::new();
    INSTANCE_ID.get_or_init(|| Uuid::new_v4().to_string())
}

fn digest_bridge_token(token: &str) -> [u8; 32] {
    let digest = Sha256::digest(token.as_bytes());
    let mut output = [0_u8; 32];
    output.copy_from_slice(&digest);
    output
}

fn redacted_result_url(result: &Value, key: &str) -> Value {
    result
        .get(key)
        .and_then(Value::as_str)
        .map(bridge_redact_url_for_public_readback)
        .map(Value::String)
        .unwrap_or(Value::Null)
}

fn redacted_result_title(result: &Value, title_key: &str, url_key: &str) -> Value {
    let Some(title) = result.get(title_key).and_then(Value::as_str) else {
        return Value::Null;
    };
    let url = result
        .get(url_key)
        .and_then(Value::as_str)
        .unwrap_or_default();
    Value::String(bridge_redact_title_for_public_url_readback(
        url,
        title.to_owned(),
    ))
}

fn bridge_redact_url_for_public_readback(url: &str) -> String {
    const REDACTED: &str = "redacted";
    if url.is_empty() {
        return String::new();
    }
    if let Some(rest) = bridge_strip_scheme_prefix(url, "view-source") {
        return format!(
            "view-source:{}",
            bridge_redact_url_for_public_readback(rest)
        );
    }
    for scheme in ["data", "javascript", "mailto", "blob", "filesystem"] {
        if bridge_strip_scheme_prefix(url, scheme).is_some() {
            return format!("{scheme}:{REDACTED}");
        }
    }
    if bridge_strip_scheme_prefix(url, "file").is_some() {
        return format!("file:{REDACTED}");
    }
    if let Some(rest) = bridge_strip_scheme_prefix(url, "about") {
        return bridge_redact_about_url(rest);
    }
    match reqwest::Url::parse(url) {
        Ok(parsed) => bridge_redact_parsed_url(parsed),
        Err(_error) => bridge_redact_query_and_fragment_without_parse(url),
    }
}

fn bridge_redact_title_for_public_url_readback(url: &str, title: String) -> String {
    const REDACTED: &str = "redacted";
    if bridge_url_has_content_bearing_scheme(url) {
        return REDACTED.to_owned();
    }
    bridge_redact_url_like_title_for_public_readback(&title).unwrap_or(title)
}

fn bridge_url_has_content_bearing_scheme(url: &str) -> bool {
    if let Some(rest) = bridge_strip_scheme_prefix(url, "view-source") {
        return bridge_url_has_content_bearing_scheme(rest);
    }
    ["data", "javascript", "mailto", "file", "blob", "filesystem"]
        .iter()
        .any(|scheme| bridge_strip_scheme_prefix(url, scheme).is_some())
}

fn bridge_strip_scheme_prefix<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    let (candidate, rest) = url.split_once(':')?;
    candidate.eq_ignore_ascii_case(scheme).then_some(rest)
}

fn bridge_redact_parsed_url(mut parsed: reqwest::Url) -> String {
    const REDACTED: &str = "redacted";
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    if parsed.path() != "/" && !parsed.path().is_empty() {
        parsed.set_path(&format!("/{REDACTED}"));
    }
    if parsed.query().is_some() {
        parsed.set_query(Some(REDACTED));
    }
    if parsed.fragment().is_some() {
        parsed.set_fragment(Some(REDACTED));
    }
    parsed.to_string()
}

fn bridge_redact_about_url(rest: &str) -> String {
    const REDACTED: &str = "redacted";
    if rest.eq_ignore_ascii_case("blank") {
        return "about:blank".to_owned();
    }
    if rest
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("blank?"))
    {
        return format!("about:blank?{REDACTED}");
    }
    if rest
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("blank#"))
    {
        return format!("about:blank#{REDACTED}");
    }
    format!("about:{REDACTED}")
}

fn bridge_redact_query_and_fragment_without_parse(url: &str) -> String {
    const REDACTED: &str = "redacted";
    let query_index = url.find('?');
    let fragment_index = url.find('#');
    match (query_index, fragment_index) {
        (None, None) => bridge_redact_unparseable_path(url),
        (Some(query), None) => {
            format!(
                "{}?{REDACTED}",
                bridge_redact_unparseable_path(&url[..query])
            )
        }
        (None, Some(fragment)) => {
            format!(
                "{}#{REDACTED}",
                bridge_redact_unparseable_path(&url[..fragment])
            )
        }
        (Some(query), Some(fragment)) if query < fragment => format!(
            "{}?{REDACTED}#{REDACTED}",
            bridge_redact_unparseable_path(&url[..query])
        ),
        (Some(_query), Some(fragment)) => format!(
            "{}#{REDACTED}",
            bridge_redact_unparseable_path(&url[..fragment])
        ),
    }
}

fn bridge_redact_unparseable_path(path: &str) -> String {
    const REDACTED: &str = "redacted";
    if path.is_empty() || path == "/" {
        return path.to_owned();
    }
    if path.starts_with('/') {
        return format!("/{REDACTED}");
    }
    REDACTED.to_owned()
}

fn bridge_redact_url_like_title_for_public_readback(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() || trimmed != title {
        return None;
    }
    if reqwest::Url::parse(trimmed).is_ok() {
        let public = bridge_redact_url_for_public_readback(trimmed);
        return (public != trimmed).then_some(public);
    }
    if !bridge_looks_like_bare_url_path(trimmed) {
        return None;
    }
    let parsed = reqwest::Url::parse(&format!("https://{trimmed}")).ok()?;
    let public = bridge_redact_parsed_url(parsed);
    public.strip_prefix("https://").map(ToOwned::to_owned)
}

fn bridge_looks_like_bare_url_path(title: &str) -> bool {
    if title.chars().any(char::is_whitespace) {
        return false;
    }
    let Some(separator) = title.find(['/', '?', '#']) else {
        return false;
    };
    let host = &title[..separator];
    !host.is_empty()
        && host.contains('.')
        && host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.'))
}

fn chrome_response_readback_summary(kind: &str, result: Option<&Value>) -> Option<String> {
    let result = result?;
    let summary = match kind {
        "openTab" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "chrome_window_focused": result.get("chrome_window_focused"),
            "chrome_window_state": result.get("chrome_window_state"),
            "chrome_window_selection_reason": result.get("chrome_window_selection_reason"),
            "chrome_window_candidate_count": result.get("chrome_window_candidate_count"),
            "chrome_window_non_focused_count": result.get("chrome_window_non_focused_count"),
            "url": redacted_result_url(result, "url"),
            "target_attached": result.get("target_attached"),
            "target_count_before": result.get("target_count_before"),
            "target_count_after": result.get("target_count_after"),
            "extension_id": result.get("extension_id"),
        }),
        "closeTab" | "operatorPanicCloseTab" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "target_count_before": result.get("target_count_before"),
            "target_count_after": result.get("target_count_after"),
            "extension_id": result.get("extension_id"),
            "operator_panic_cleanup": result.get("operator_panic_cleanup"),
            "expected_disable_sequence": result.get("expected_disable_sequence"),
        }),
        "navigateTab" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "action": result.get("action"),
            "requested_url": redacted_result_url(result, "requested_url"),
            "before_url": redacted_result_url(result, "before_url"),
            "after_url": redacted_result_url(result, "after_url"),
            "ready_state": result.get("ready_state"),
            "readback_backend": result.get("readback_backend"),
        }),
        "activateTab" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "before_active": result.get("before_active"),
            "active": result.get("active"),
            "readback_backend": result.get("readback_backend"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "capturePageScreenshot" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "chrome_window_focused": result.get("chrome_window_focused"),
            "chrome_window_state": result.get("chrome_window_state"),
            "url": redacted_result_url(result, "url"),
            "title": redacted_result_title(result, "title", "url"),
            "ready_state": result.get("ready_state"),
            "before_active": result.get("before_active"),
            "active_for_capture": result.get("active_for_capture"),
            "previous_active_tab_id": result.get("previous_active_tab_id"),
            "restored_previous_active": result.get("restored_previous_active"),
            "image_format": result.get("image_format"),
            "image_data_url_len": result.get("image_data_url_len"),
            "readback_backend": result.get("readback_backend"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "pageScreenshot" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "scope": result.get("scope"),
            "image_format": result.get("image_format"),
            "quality": result.get("quality"),
            "omit_background": result.get("omit_background"),
            "clip_css": result.get("clip_css"),
            "output_css_width": result.get("output_css_width"),
            "output_css_height": result.get("output_css_height"),
            "device_pixel_ratio": result.get("device_pixel_ratio"),
            "tile_count": result.get("tile_count"),
            "mask_count": result.get("mask_count"),
            "before_active": result.get("before_active"),
            "active_for_capture": result.get("active_for_capture"),
            "restored_previous_active": result.get("restored_previous_active"),
            "readback_backend": result.get("readback_backend"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "pagePdf" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "pdf_byte_length": result.get("pdf_byte_length"),
            "landscape": result.get("landscape"),
            "print_background": result.get("print_background"),
            "paper_width": result.get("paper_width"),
            "paper_height": result.get("paper_height"),
            "margin_top": result.get("margin_top"),
            "margin_bottom": result.get("margin_bottom"),
            "margin_left": result.get("margin_left"),
            "margin_right": result.get("margin_right"),
            "readback_backend": result.get("readback_backend"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "downloads" => json!({
            "operation": result.get("operation"),
            "returned": result.get("returned"),
            "event_count": result.get("event_count"),
            "condition_met": result.get("condition_met"),
            "timed_out": result.get("timed_out"),
            "elapsed_ms": result.get("elapsed_ms"),
            "timeout_ms": result.get("timeout_ms"),
            "selected_download_id": result
                .get("selected_item")
                .and_then(|value| value.get("id")),
            "selected_state": result
                .get("selected_item")
                .and_then(|value| value.get("state")),
            "selected_bytes_received": result
                .get("selected_item")
                .and_then(|value| value.get("bytes_received")),
            "readback_backend": result.get("readback_backend"),
            "extension_id": result.get("extension_id"),
        }),
        "evaluateScript" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "scope": result.get("scope"),
            "result_type": result.get("result_type"),
            "result_subtype": result.get("result_subtype"),
            "returned_by_value": result.get("returned_by_value"),
            "readback_backend": result.get("readback_backend"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "initScript" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "operation": result.get("operation"),
            "identifier": result.get("identifier"),
            "readback_backend": result.get("readback_backend"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "exposeBinding" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "operation": result.get("operation"),
            "name": result.get("name"),
            "newly_armed": result.get("newly_armed"),
            "binding_newly_added": result.get("binding_newly_added"),
            "binding_removed": result.get("binding_removed"),
            "binding_active": result.get("binding_active"),
            "active_binding_count": result.get("active_binding_count"),
            "returned": result.get("returned"),
            "total_buffered": result.get("total_buffered"),
            "next_cursor": result.get("next_cursor"),
            "dropped": result.get("dropped"),
            "readback_backend": result.get("readback_backend"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "handleDialog" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "operation": result.get("operation"),
            "default_policy": result.get("default_policy"),
            "capture_newly_armed": result.get("capture_newly_armed"),
            "handled": result.get("handled"),
            "handle_action": result.get("handle_action"),
            "pending_type": result
                .get("pending_dialog")
                .and_then(|value| value.get("dialog_type")),
            "pending_message": result
                .get("pending_dialog")
                .and_then(|value| value.get("message")),
            "returned": result.get("returned"),
            "total_buffered": result.get("total_buffered"),
            "next_cursor": result.get("next_cursor"),
            "opened_count": result.get("opened_count"),
            "closed_count": result.get("closed_count"),
            "auto_handled_count": result.get("auto_handled_count"),
            "error_count": result.get("error_count"),
            "readback_backend": result.get("readback_backend"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "fileUpload" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "operation": result.get("operation"),
            "requested_file_count": result.get("requested_file_count"),
            "input_file_count": result
                .get("input")
                .and_then(|value| value.get("file_count")),
            "input_names": result
                .get("input")
                .and_then(|value| value.get("files")),
            "capture_newly_armed": result.get("capture_newly_armed"),
            "pending_mode": result
                .get("pending_chooser")
                .and_then(|value| value.get("mode")),
            "pending_backend_node_id": result
                .get("pending_chooser")
                .and_then(|value| value.get("backend_node_id")),
            "handled_seq": result
                .get("handled_chooser")
                .and_then(|value| value.get("seq")),
            "returned": result.get("returned"),
            "total_buffered": result.get("total_buffered"),
            "next_cursor": result.get("next_cursor"),
            "opened_count": result.get("opened_count"),
            "handled_count": result.get("handled_count"),
            "canceled_count": result.get("canceled_count"),
            "error_count": result.get("error_count"),
            "readback_backend": result.get("readback_backend"),
            "chooser_readback_backend": result.get("chooser_readback_backend"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "pageVitals" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "url": redacted_result_url(result, "url"),
            "title": redacted_result_title(result, "title", "url"),
            "ready_state": result.get("ready_state"),
            "viewport_inner_width": result
                .get("viewport")
                .and_then(|value| value.get("inner_width")),
            "viewport_inner_height": result
                .get("viewport")
                .and_then(|value| value.get("inner_height")),
            "viewport_device_pixel_ratio": result
                .get("viewport")
                .and_then(|value| value.get("device_pixel_ratio")),
            "viewport_scroll_width": result
                .get("viewport")
                .and_then(|value| value.get("scroll_width")),
            "viewport_scroll_height": result
                .get("viewport")
                .and_then(|value| value.get("scroll_height")),
            "viewport_error_present": result.get("viewport_error_detail").is_some(),
            "visibility_state": result
                .get("page_vitals")
                .and_then(|value| value.get("visibility_state")),
            "document_hidden": result
                .get("page_vitals")
                .and_then(|value| value.get("document_hidden")),
            "lcp_supported": result
                .get("page_vitals")
                .and_then(|value| value.get("lcp_supported")),
            "lcp_entry_count": result
                .get("page_vitals")
                .and_then(|value| value.get("lcp_entry_count")),
            "lcp_size": result
                .get("page_vitals")
                .and_then(|value| value.get("lcp"))
                .and_then(|value| value.get("size")),
            "readback_backend": result.get("readback_backend"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "targetInfo" | "targetInfoPageText" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "target_type": result.get("target_type"),
            "url": redacted_result_url(result, "url"),
            "title": redacted_result_title(result, "title", "url"),
            "page_text_available": result
                .get("page_text")
                .and_then(|value| value.get("available")),
            "page_text_len": result
                .get("page_text")
                .and_then(|value| value.get("text_len")),
            "page_text_truncated": result
                .get("page_text")
                .and_then(|value| value.get("text_truncated")),
            "page_text_readback_scope": result
                .get("page_text")
                .and_then(|value| value.get("readback_scope")),
            "page_text_frame_count": result
                .get("page_text")
                .and_then(|value| value.get("frame_count")),
            "page_text_nonempty_frame_count": result
                .get("page_text")
                .and_then(|value| value.get("frame_text_nonempty_count")),
            "page_vitals_available": result
                .get("page_vitals")
                .and_then(|value| value.get("available")),
            "page_vitals_visibility_state": result
                .get("page_vitals")
                .and_then(|value| value.get("visibility_state")),
            "page_vitals_lcp_entry_count": result
                .get("page_vitals")
                .and_then(|value| value.get("lcp_entry_count")),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "viewportEmulation" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "operation": result.get("operation"),
            "requested": result.get("requested"),
            "inner_width": result
                .get("viewport")
                .and_then(|value| value.get("inner_width")),
            "inner_height": result
                .get("viewport")
                .and_then(|value| value.get("inner_height")),
            "device_pixel_ratio": result
                .get("viewport")
                .and_then(|value| value.get("device_pixel_ratio")),
            "readback_backend": result.get("readback_backend"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "deviceEmulation" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "operation": result.get("operation"),
            "descriptor": result.get("descriptor"),
            "restored_user_agent": result.get("restored_user_agent"),
            "inner_width": result
                .get("device")
                .and_then(|value| value.get("viewport"))
                .and_then(|value| value.get("inner_width")),
            "inner_height": result
                .get("device")
                .and_then(|value| value.get("viewport"))
                .and_then(|value| value.get("inner_height")),
            "device_pixel_ratio": result
                .get("device")
                .and_then(|value| value.get("viewport"))
                .and_then(|value| value.get("device_pixel_ratio")),
            "max_touch_points": result
                .get("device")
                .and_then(|value| value.get("max_touch_points")),
            "pointer_coarse": result
                .get("device")
                .and_then(|value| value.get("pointer_coarse")),
            "readback_backend": result.get("readback_backend"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "geolocationEmulation" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "operation": result.get("operation"),
            "origin": result.get("origin"),
            "requested": result.get("requested"),
            "permission_setting": result.get("permission_setting"),
            "permission_state": result
                .get("geolocation")
                .and_then(|value| value.get("permission_state")),
            "position_returned": result
                .get("geolocation")
                .and_then(|value| value.get("position"))
                .map(|value| !value.is_null()),
            "error_code": result
                .get("geolocation")
                .and_then(|value| value.get("error"))
                .and_then(|value| value.get("code")),
            "readback_backend": result.get("readback_backend"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "localeEmulation" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "operation": result.get("operation"),
            "requested": result.get("requested"),
            "locale": result
                .get("locale")
                .and_then(|value| value.get("locale")),
            "time_zone": result
                .get("locale")
                .and_then(|value| value.get("time_zone")),
            "sample_number": result
                .get("locale")
                .and_then(|value| value.get("sample_number")),
            "sample_date": result
                .get("locale")
                .and_then(|value| value.get("sample_date")),
            "readback_backend": result.get("readback_backend"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "mediaEmulation" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "operation": result.get("operation"),
            "requested": result.get("requested"),
            "media_screen": result
                .get("media")
                .and_then(|value| value.get("media_screen")),
            "media_print": result
                .get("media")
                .and_then(|value| value.get("media_print")),
            "color_scheme_dark": result
                .get("media")
                .and_then(|value| value.get("color_scheme_dark")),
            "reduced_motion_reduce": result
                .get("media")
                .and_then(|value| value.get("reduced_motion_reduce")),
            "readback_backend": result.get("readback_backend"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "networkConditions" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "operation": result.get("operation"),
            "requested": result.get("requested"),
            "online": result
                .get("network")
                .and_then(|value| value.get("online")),
            "connection_type": result
                .get("network")
                .and_then(|value| value.get("connection_type")),
            "effective_type": result
                .get("network")
                .and_then(|value| value.get("effective_type")),
            "downlink_mbps": result
                .get("network")
                .and_then(|value| value.get("downlink_mbps")),
            "rtt_ms": result
                .get("network")
                .and_then(|value| value.get("rtt_ms")),
            "readback_backend": result.get("readback_backend"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "typeActiveElement" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chars_typed": result.get("chars_typed"),
            "readback_backend": result.get("readback_backend"),
            "frame_id": result.get("frame_id"),
            "frame_result_count": result.get("frame_result_count"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "setFieldValue" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "resolved_by": result.get("resolved_by"),
            "match_count": result.get("match_count"),
            "tag_name": result.get("tag_name"),
            "readback_backend": result.get("readback_backend"),
            "frame_id": result.get("frame_id"),
            "frame_result_count": result.get("frame_result_count"),
            "before_value_len": result.get("before_value_len"),
            "after_value_len": result.get("after_value_len"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "pageContent" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "url": redacted_result_url(result, "url"),
            "title": redacted_result_title(result, "title", "url"),
            "ready_state": result.get("ready_state"),
            "html_len": result.get("html_len"),
            "truncated": result.get("truncated"),
            "max_bytes": result.get("max_bytes"),
            "readback_backend": result.get("readback_backend"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "setContent" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "before_url": redacted_result_url(result, "before_url"),
            "after_url": redacted_result_url(result, "after_url"),
            "after_title": redacted_result_title(result, "after_title", "after_url"),
            "ready_state": result.get("ready_state"),
            "html_len": result.get("html_len"),
            "readback_backend": result.get("readback_backend"),
            "backend_tier_used": result.get("backend_tier_used"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "locateElements" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "engine": result.get("engine"),
            "query": result.get("query"),
            "match_count": result.get("match_count"),
            "returned_count": result.get("returned_count"),
            "truncated": result.get("truncated"),
            "readback_backend": result.get("readback_backend"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "inspectElement" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "element_id": result.get("element_id"),
            "tag_name": result.get("element").and_then(|value| value.get("tag_name")),
            "is_visible": result.get("element").and_then(|value| value.get("is_visible")),
            "is_enabled": result.get("element").and_then(|value| value.get("is_enabled")),
            "action_ready": result
                .get("element")
                .and_then(|value| value.get("actionability"))
                .and_then(|value| value.get("action_ready")),
            "receives_events": result
                .get("element")
                .and_then(|value| value.get("actionability"))
                .and_then(|value| value.get("receives_events")),
            "readback_backend": result.get("readback_backend"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "scrollIntoView" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "element_id": result.get("element_id"),
            "window_scroll_changed": result
                .get("scroll")
                .and_then(|value| value.get("window_scroll_changed")),
            "container_scroll_changed": result
                .get("scroll")
                .and_then(|value| value.get("container_scroll_changed")),
            "node_fully_in_viewport_after": result
                .get("scroll")
                .and_then(|value| value.get("node_fully_in_viewport_after")),
            "readback_backend": result.get("readback_backend"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "clock" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "operation": result.get("operation"),
            "init_script_identifier": result.get("init_script_identifier"),
            "init_script_newly_added": result.get("init_script_newly_added"),
            "installed_at_unix_ms": result.get("installed_at_unix_ms"),
            "readback": result.get("readback"),
            "url": redacted_result_url(result, "url"),
            "title": redacted_result_title(result, "title", "url"),
            "ready_state": result.get("ready_state"),
            "readback_backend": result.get("readback_backend"),
            "backend_tier_used": result.get("backend_tier_used"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "pageEvents" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "chrome_window_id": result.get("chrome_window_id"),
            "capture_newly_armed": result.get("capture_newly_armed"),
            "next_cursor": result.get("next_cursor"),
            "returned": result.get("returned"),
            "total_buffered": result.get("total_buffered"),
            "dropped": result.get("dropped"),
            "filters": result.get("filters"),
            "entry_count": result.get("entries").and_then(Value::as_array).map(Vec::len),
            "page_count": result.get("pages").and_then(Value::as_array).map(Vec::len),
            "worker_count": result.get("workers").and_then(Value::as_array).map(Vec::len),
            "readback_backend": result.get("readback_backend"),
            "backend_tier_used": result.get("backend_tier_used"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "domAction" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "action": result.get("action"),
            "event_type": result
                .get("action_readback")
                .and_then(|value| value.get("event_type")),
            "default_allowed": result
                .get("action_readback")
                .and_then(|value| value.get("default_allowed")),
            "matched_count": result.get("matched_count"),
            "resolved_by": result.get("resolved_by"),
            "auto_wait": result.get("auto_wait"),
            "auto_wait_poll_count": result.get("auto_wait_readback")
                .and_then(|value| value.get("poll_count")),
            "auto_wait_unmet_predicates": result.get("auto_wait_readback")
                .and_then(|value| value.get("unmet_predicates")),
            "readback_backend": result.get("readback_backend"),
            "tab_activation_for_touch": result.get("tab_activation_for_touch"),
            "frame_id": result.get("frame_id"),
            "frame_result_count": result.get("frame_result_count"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "cdpInput" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "action": result.get("action"),
            "method": result.get("method"),
            "viewport_point": result.get("viewport_point"),
            "resolved_by": result.get("resolved_by"),
            "matched_count": result.get("matched_count"),
            "auto_wait": result.get("auto_wait"),
            "auto_wait_poll_count": result.get("auto_wait_readback")
                .and_then(|value| value.get("poll_count")),
            "auto_wait_unmet_predicates": result.get("auto_wait_readback")
                .and_then(|value| value.get("unmet_predicates")),
            "readback_backend": result.get("readback_backend"),
            "frame_id": result.get("frame_id"),
            "frame_result_count": result.get("frame_result_count"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "coordinateClick" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "coordinate_space": result.get("coordinate_space"),
            "input_coordinate": result.get("input_coordinate"),
            "viewport_coordinate": result.get("viewport_coordinate"),
            "click_count": result.get("click_count"),
            "before_element": result.get("before_element"),
            "after_element": result.get("after_element"),
            "active_element": result.get("active_element"),
            "readback_backend": result.get("readback_backend"),
            "target_candidate_count": result.get("target_candidate_count"),
            "target_selection_reason": result.get("target_selection_reason"),
            "extension_id": result.get("extension_id"),
        }),
        "maintenancePauseReconnect" => json!({
            "host_id": result.get("host_id"),
            "pause_ms": result.get("pause_ms"),
            "pause_until_unix_ms": result.get("pause_until_unix_ms"),
            "reason": result.get("reason"),
            "reconnect_suppressed": result.get("reconnect_suppressed"),
            "bridge_build_id": result.get("bridge_build_id"),
            "extension_id": result.get("extension_id"),
        }),
        _ => return None,
    };
    serde_json::to_string(&summary).ok()
}

fn bridge_token_from_query(query: &str) -> Option<&str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if key == "bridge_token" {
            Some(value)
        } else {
            None
        }
    })
}

fn replace_active_direct_http_host(inner: &mut BridgeInner, new_host_id: &str) {
    let Some(old_host_id) = inner.active_host_id.clone() else {
        return;
    };
    if old_host_id == new_host_id {
        return;
    }
    let Some(old_host) = inner.hosts.get(&old_host_id) else {
        return;
    };
    if old_host.transport.as_deref() != Some("direct_http") {
        return;
    }
    let terminal_detail = format!(
        "Chrome debugger direct HTTP bridge host {old_host_id} was replaced by {new_host_id} before command response"
    );
    let stats =
        preserve_delivered_mutations_on_transport_loss(inner, &old_host_id, &terminal_detail);
    tracing::warn!(
        code = "CHROME_DEBUGGER_DIRECT_HTTP_HOST_REPLACED",
        old_host_id = %old_host_id,
        new_host_id = %new_host_id,
        queued_removed = stats.queued_removed,
        pending_failed_before_delivery = stats.pending_failed_before_delivery,
        delivered_commands_preserved = stats.delivered_commands_preserved,
        "Chrome debugger direct HTTP bridge host replaced"
    );
}
