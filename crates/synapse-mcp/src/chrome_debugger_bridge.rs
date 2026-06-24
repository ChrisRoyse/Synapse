use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    ffi::OsString,
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use axum::{
    Json,
    extract::{
        Query,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
#[cfg(test)]
use synapse_core::CdpStatus;
use synapse_core::{Rect, SubsystemHealth, error_codes};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Notify, RwLock, oneshot},
    time::{sleep, timeout},
};
use uuid::Uuid;

const EXTENSION_ID: &str = "leoocgnkjnplbfdbklajepahofecgfbk";
const NATIVE_HOST_NAME: &str = "com.synapse.chrome_debugger";
const EXTENSION_ORIGIN: &str = "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk";
const BRIDGE_TOKEN_HEADER: &str = "x-synapse-bridge-token";
const BRIDGE_PROTOCOL_VERSION: u32 = 1;
const EXPECTED_EXTENSION_BUILD_ID: &str = "synapse-chrome-bridge-2026-06-23-file-upload-v2";
const EXPECTED_EXTENSION_BUILD_SHA256: &str =
    "6a557863fa6472214e3c2f521a52079209c9a7d23f209f49b7c1647cc48e0b95";
const SYNAPSE_CHROME_BLOCKED_INSTALL_MESSAGE: &str = "Synapse blocked this extension on this host because debugger/nativeMessaging permissions can surface Chrome debugger or native-host popups during background automation.";
const REQUIRED_DIRECT_HTTP_CAPABILITIES: &[&str] = &[
    "alarmReconnect",
    "activateTab",
    "closeTab",
    "coordinateClick",
    "cookies",
    "downloads",
    "domAction",
    "externalPopupRiskSuppression",
    "frameLocators",
    "frames",
    "listTabs",
    "navigateTab",
    "openTab",
    "pageVitals",
    "pageContent",
    "pageScreenshot",
    "pagePdf",
    "setContent",
    "storageState",
    "ariaSnapshot",
    "assertPoll",
    "locateElements",
    "inspectElement",
    "scrollIntoView",
    "waitForText",
    "waitForFunction",
    "waitForLoadState",
    "waitForUrl",
    "waitForRequest",
    "waitForResponse",
    "waitForSelector",
    "clock",
    "pageEvents",
    "evaluateScript",
    "initScript",
    "exposeBinding",
    "handleDialog",
    "fileUpload",
    "cdpInput",
    "viewportEmulation",
    "deviceEmulation",
    "geolocationEmulation",
    "localeEmulation",
    "mediaEmulation",
    "networkConditions",
    "reloadSelf",
    "targetInfo",
    "targetInfoPageText",
    "typeActiveElement",
    "setFieldValue",
];
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const NATIVE_POLL_TIMEOUT: Duration = Duration::from_secs(15);
const DIRECT_WS_COMMAND_WAIT: Duration = Duration::from_secs(25);
const DEFAULT_RELOAD_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_RELOAD_WAIT_TIMEOUT_MS: u64 = 30_000;
const RELOAD_RECONNECT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const NATIVE_DAEMON_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_NATIVE_MESSAGE_FROM_CHROME: usize = 64 * 1024 * 1024;
const MAX_NATIVE_MESSAGE_TO_CHROME: usize = 1024 * 1024;
const UNKNOWN_NATIVE_HOST_ID_FRAGMENT: &str = "unknown chrome debugger native host_id";
const INSTALL_GUIDANCE: &str = "install the bundled Synapse Chrome extension with scripts\\install-synapse-chrome-debugger.ps1; the installer deploys the bridge to %LOCALAPPDATA%\\synapse\\chrome-extension\\<build-id> and auto-loads that stable unpacked directory into the already-open active Chrome profile while refusing to launch a second Chrome profile; the normal end-user bridge uses chrome.tabs/chrome.scripting/chrome.downloads/chrome.webNavigation/chrome.webRequest over direct localhost WebSocket plus chrome.alarms MV3 reconnect wake, exposes debugger-free pageScreenshot capture through chrome.tabs.captureVisibleTab stitching, exposes chrome.downloads list/wait/event capture for browser_downloads save/move, and exposes narrow chrome.debugger lanes for target-scoped hover/tap/active-tab drag, Page.printToPDF PDF rendering, Runtime.evaluate page evaluation, Page.addScriptToEvaluateOnNewDocument init scripts, Runtime.addBinding/Runtime.bindingCalled binding capture, Page.handleJavaScriptDialog dialog handling, DOM.setFileInputFiles/Page.fileChooserOpened file upload handling, viewport emulation, device emulation, geolocation emulation, locale/timezone emulation, media emulation, and network conditions plus inactive-tab synthetic mouse drag and HTML5 DataTransfer drag dispatch; it never uses nativeMessaging or helper Chrome windows; expected_extension_id=leoocgnkjnplbfdbklajepahofecgfbk";
const NO_ACTIVE_HOST_REPAIR_GUIDANCE: &str = "no_active_host_repair=use the already-open authenticated Chrome profile only; do not launch a second Chrome process/profile; wait for the installed bridge worker alarmReconnect registration and re-read health; if an active stale host appears call cdp_bridge_reload; if no host registers, run scripts\\install-synapse-chrome-debugger.ps1 from the interactive Windows desktop so it auto-loads the bundled unpacked extension into the existing active Chrome profile; if health reports installed=false, cdp_bridge_reload cannot repair because Chrome has no loaded extension host to receive reloadSelf";
const TOKEN_ENV: &str = "SYNAPSE_BEARER_TOKEN";
const APPDATA_ENV: &str = "APPDATA";

#[derive(Clone, Debug)]
pub(crate) struct NativeHostInvocation {
    pub origin: String,
    pub parent_window: Option<String>,
}

#[must_use]
pub(crate) fn native_host_invocation_from_args<I>(args: I) -> Option<NativeHostInvocation>
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
pub(crate) struct ChromeDebuggerBridgeError {
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

    #[must_use]
    #[cfg(test)]
    pub(crate) fn cdp_status(&self) -> CdpStatus {
        if self.code == error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE
            || self.code == error_codes::CHROME_BRIDGE_EXTENSION_STALE
        {
            CdpStatus::ExtensionUnavailable
        } else {
            CdpStatus::AttachFailed
        }
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

    fn extension(code: Option<&str>, detail: impl Into<String>) -> Self {
        let code = match code {
            Some(error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE) => {
                error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE
            }
            Some(error_codes::A11Y_CDP_EXTENSION_DETACHED) => {
                error_codes::A11Y_CDP_EXTENSION_DETACHED
            }
            Some(error_codes::A11Y_CDP_EXTENSION_TIMEOUT) => {
                error_codes::A11Y_CDP_EXTENSION_TIMEOUT
            }
            Some(error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED) => {
                error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
            }
            Some(error_codes::CHROME_BRIDGE_EXTENSION_STALE) => {
                error_codes::CHROME_BRIDGE_EXTENSION_STALE
            }
            Some(error_codes::A11Y_CDP_AXTREE_FAILED) => error_codes::A11Y_CDP_AXTREE_FAILED,
            Some(error_codes::A11Y_CDP_ATTACH_FAILED) => error_codes::A11Y_CDP_ATTACH_FAILED,
            Some(error_codes::CHROME_SCRIPTING_EXECUTE_FAILED) => {
                error_codes::CHROME_SCRIPTING_EXECUTE_FAILED
            }
            Some(error_codes::CHROME_DOM_SELECTOR_INVALID) => {
                error_codes::CHROME_DOM_SELECTOR_INVALID
            }
            Some(error_codes::CHROME_DOM_ELEMENT_NOT_FOUND) => {
                error_codes::CHROME_DOM_ELEMENT_NOT_FOUND
            }
            Some(error_codes::CHROME_DOM_ELEMENT_AMBIGUOUS) => {
                error_codes::CHROME_DOM_ELEMENT_AMBIGUOUS
            }
            Some(error_codes::CHROME_DOM_ELEMENT_NOT_ACTIONABLE) => {
                error_codes::CHROME_DOM_ELEMENT_NOT_ACTIONABLE
            }
            Some(error_codes::CHROME_DOM_ACTION_UNSUPPORTED) => {
                error_codes::CHROME_DOM_ACTION_UNSUPPORTED
            }
            Some(error_codes::CHROME_DOM_ACTION_POSTCONDITION_FAILED) => {
                error_codes::CHROME_DOM_ACTION_POSTCONDITION_FAILED
            }
            Some(error_codes::ACTION_TARGET_INVALID) => error_codes::ACTION_TARGET_INVALID,
            Some(error_codes::BROWSER_WAIT_TIMEOUT) => error_codes::BROWSER_WAIT_TIMEOUT,
            _ => error_codes::A11Y_CDP_ATTACH_FAILED,
        };
        Self {
            code,
            detail: detail.into(),
        }
    }

    fn stale(command_kind: &str, host_id: &str, host: &HostRecord, reason: &str) -> Self {
        Self {
            code: error_codes::CHROME_BRIDGE_EXTENSION_STALE,
            detail: format!(
                "Chrome bridge extension is stale for command {command_kind:?}; host_id={host_id} reason={reason} extension_id={} extension_version={} extension_protocol_version={} extension_build_id={} extension_build_sha256={} capabilities={} expected_build_id={} expected_build_sha256={} required_capabilities={} remediation=run scripts\\install-synapse-chrome-debugger.ps1 to deploy the bundled bridge into the stable %LOCALAPPDATA%\\synapse\\chrome-extension\\<build-id> directory, then call cdp_bridge_reload from a bridge that advertises reloadSelf; if the loaded worker predates reloadSelf, fail closed and wait for a Chrome restart/reload rather than using foreground chrome://extensions automation",
                host.extension_id.as_deref().unwrap_or("not_seen_yet"),
                host.extension_version.as_deref().unwrap_or("not_seen_yet"),
                host.extension_protocol_version
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "not_seen_yet".to_owned()),
                host.extension_build_id.as_deref().unwrap_or("not_seen_yet"),
                host.extension_build_sha256
                    .as_deref()
                    .unwrap_or("not_seen_yet"),
                format_capabilities(&host.extension_capabilities),
                EXPECTED_EXTENSION_BUILD_ID,
                EXPECTED_EXTENSION_BUILD_SHA256,
                REQUIRED_DIRECT_HTTP_CAPABILITIES.join(",")
            ),
        }
    }

    fn normal_bridge_attach_disabled(hwnd: i64, command_kind: &str) -> Self {
        let external_surface_hint = external_chrome_surface_hint();
        Self {
            code: error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
            detail: format!(
                "Synapse Chrome Bridge refused unsupported attach-capable command {command_kind:?} before queueing any Chrome command; hwnd={hwnd} reason=current normal-profile bridge exposes only narrow chrome.debugger lanes for Runtime.evaluate page eval, Page.addScriptToEvaluateOnNewDocument init scripts, cdpInput target-scoped hover/tap/active-tab drag, viewportEmulation, deviceEmulation, geolocationEmulation, localeEmulation, mediaEmulation, and networkConditions plus inactive-tab synthetic mouse drag, while this command still requires a dedicated raw-CDP automation profile{external_surface_hint} remediation=run scripts\\install-synapse-chrome-debugger.ps1 and cdp_bridge_reload to ensure the current bridge is installed, or use raw CDP from a dedicated Synapse-launched automation profile for full DOM/action CDP or screenshots"
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
    let mut rows = external_chrome_profile_surfaces();
    rows.extend(external_chrome_native_messaging_processes());
    rows.sort();
    rows.dedup();
    rows
}

fn synapse_chrome_self_profile_surfaces() -> Vec<String> {
    let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") else {
        return Vec::new();
    };
    let user_data_root = PathBuf::from(local_appdata)
        .join("Google")
        .join("Chrome")
        .join("User Data");
    let Ok(profile_dirs) = std::fs::read_dir(user_data_root) else {
        return Vec::new();
    };

    let mut rows = Vec::new();
    for profile_dir in profile_dirs.flatten() {
        let Ok(file_type) = profile_dir.file_type() else {
            continue;
        };
        if !file_type.is_dir() || profile_dir.file_name() == "Snapshots" {
            continue;
        }
        let profile = profile_dir.file_name().to_string_lossy().into_owned();
        let mut runtime_by_id: HashMap<String, ChromeExtensionRuntimeState> = HashMap::new();
        for pref_file in ["Preferences", "Secure Preferences"] {
            let pref_path = profile_dir.path().join(pref_file);
            let Ok(raw) = std::fs::read_to_string(&pref_path) else {
                continue;
            };
            let Ok(pref) = serde_json::from_str::<Value>(&raw) else {
                rows.push(format!(
                    "profile={profile} pref={pref_file} parse_error=true"
                ));
                continue;
            };
            let Some(setting) = pref
                .get("extensions")
                .and_then(|value| value.get("settings"))
                .and_then(|settings| settings.get(EXTENSION_ID))
            else {
                continue;
            };
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
            let granted_hazards =
                synapse_self_hazard_api_permissions(granted_permissions.iter().map(String::as_str));
            if active_or_manifest_hazards.is_empty() && granted_hazards.is_empty() {
                continue;
            }
            let disabled =
                !runtime_state.disable_reasons.is_empty() || runtime_state.state == Some(0);
            let active_hazard_enabled = !disabled && !active_or_manifest_hazards.is_empty();
            let granted_only = active_or_manifest_hazards.is_empty() && !granted_hazards.is_empty();
            if !active_hazard_enabled && !granted_only {
                continue;
            }
            let risk_basis = if active_hazard_enabled {
                "active_or_manifest_hazard_without_disable_reason"
            } else {
                "granted_only_stale"
            };
            rows.push(format!(
                "profile={profile} pref={pref_file} extension_id={EXTENSION_ID} name=\"Synapse Chrome Bridge\" active_api={} manifest_api={} granted_hazard_api={} synapse_self_popup_risk=true risk_basis={risk_basis} state={} active_bit={} disable_reasons={}",
                active_permissions.join(","),
                manifest_permissions.join(","),
                granted_hazards.join(","),
                runtime_state
                    .state
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<absent>".to_owned()),
                runtime_state
                    .active_bit
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<absent>".to_owned()),
                format_disable_reasons(&runtime_state.disable_reasons)
            ));
        }
    }
    rows.sort();
    rows.dedup();
    rows
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
            "synapse_chrome_bridge_permission_blocking=true self_risk_scope=active_synapse_bridge_native_messaging_permission self_risk_count={} self_active_risk_count={} synapse_chrome_bridge_permission_risk={} remediation=normal bridge commands fail closed while the Synapse extension profile row still exposes active/manifest nativeMessaging; rerun scripts\\install-synapse-chrome-debugger.ps1 to preserve the self ExtensionSettings blocked_permissions shield and reload the existing Chrome extension through cdp_bridge_reload when available",
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
        "synapse_chrome_bridge_permission_warning=true self_risk_scope={shield_scope} self_risk_count={} synapse_chrome_bridge_permission_risk={} remediation=granted-only profile residue is not active Chrome extension capability once the loaded bridge identity is current and extension_debugger_api_available=true with cdpInput advertised; setup still attempts the HKCU ExtensionSettings self-shield for nativeMessaging when ACLs allow it, and commands rely on exact bridge identity plus active/manifest nativeMessaging fail-closed gates",
        rows.len(),
        formatted
    )
}

#[derive(Clone, Debug)]
struct SynapseChromeProfileInstallState {
    detail: String,
}

impl SynapseChromeProfileInstallState {
    fn not_scanned(reason: &str) -> Self {
        Self {
            detail: format!(
                "synapse_chrome_bridge_profile_installation scanned=false installed=unknown reason={reason}"
            ),
        }
    }
}

fn synapse_chrome_profile_install_state() -> SynapseChromeProfileInstallState {
    let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") else {
        return SynapseChromeProfileInstallState::not_scanned("localappdata_missing");
    };
    let user_data_root = PathBuf::from(local_appdata)
        .join("Google")
        .join("Chrome")
        .join("User Data");
    let active_profile_candidates = chrome_active_profile_candidates(&user_data_root);
    let Ok(profile_dirs) = std::fs::read_dir(&user_data_root) else {
        return SynapseChromeProfileInstallState {
            detail: format!(
                "synapse_chrome_bridge_profile_installation scanned=false installed=unknown user_data_root={} reason=user_data_root_unreadable",
                quote_detail_value(&user_data_root.to_string_lossy())
            ),
        };
    };

    let mut profile_count = 0_usize;
    let mut parse_error_count = 0_usize;
    let mut installed_profiles = BTreeSet::new();
    for profile_dir in profile_dirs.flatten() {
        let Ok(file_type) = profile_dir.file_type() else {
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
        for (_pref_file, pref_path) in profile_pref_paths {
            let Ok(raw) = std::fs::read_to_string(&pref_path) else {
                continue;
            };
            let Ok(pref) = serde_json::from_str::<Value>(&raw) else {
                parse_error_count += 1;
                continue;
            };
            if pref
                .get("extensions")
                .and_then(|value| value.get("settings"))
                .and_then(|settings| settings.get(EXTENSION_ID))
                .is_some()
            {
                profile_installed = true;
            }
        }
        if profile_installed {
            installed_profiles.insert(profile);
        }
    }

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
        .map(quote_detail_value)
        .unwrap_or_else(|| "<unknown>".to_owned());
    let active_profile_installed_detail = active_profile_installed
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let installed_profile_detail = if installed_profiles.is_empty() {
        "<none>".to_owned()
    } else {
        installed_profiles
            .iter()
            .map(|profile| quote_detail_value(profile))
            .collect::<Vec<_>>()
            .join(",")
    };

    SynapseChromeProfileInstallState {
        detail: format!(
            "synapse_chrome_bridge_profile_installation scanned=true installed={} user_data_root={} profile_count={} installed_profile_count={} installed_profiles={} active_profile={} active_profile_installed={} parse_error_count={} reason={} cdp_bridge_reload_can_install_absent_extension=false remediation=run scripts\\install-synapse-chrome-debugger.ps1 from the interactive Windows desktop with the target Chrome profile already open; the installer deploys the bundled bridge into %LOCALAPPDATA%\\synapse\\chrome-extension\\<build-id> and auto-loads that stable unpacked directory in the active profile. cdp_bridge_reload can only reload an already-registered bridge host and cannot install an absent Chrome extension",
            installed,
            quote_detail_value(&user_data_root.to_string_lossy()),
            profile_count,
            installed_profile_count,
            installed_profile_detail,
            active_profile_detail,
            active_profile_installed_detail,
            parse_error_count,
            reason
        ),
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
            &mut key,
        )
    };
    if status != windows::Win32::Foundation::ERROR_SUCCESS {
        return format!(
            "policy_set_value_access=false policy_set_value_access_reason=reg_open_key_set_value_failed status={} remediation=repair HKCU\\{subkey} ACL or run setup from an elevated maintenance context; until then rely on live chrome.management suppression and fail-closed command gates",
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

fn external_chrome_popup_risk_warning(rows: &[String], suppression_ok: bool) -> String {
    if rows.is_empty() {
        return "external_chrome_popup_risk_warning=false risk_count=0".to_owned();
    }
    if suppression_ok {
        return format!(
            "external_chrome_popup_risk_warning=true external_chrome_popup_risk_scope=covered_by_live_bridge_management risk_count={} external_chrome_popup_risk={} remediation=profile scan still names debugger/nativeMessaging rows, but live Chrome management readback from the installed Synapse bridge reported remaining_hazard_count=0 and failure_count=0 for the active Chrome profile; continue monitoring health and fail closed if suppression changes",
            rows.len(),
            format_external_chrome_popup_risks(rows)
        );
    }
    format!(
        "external_chrome_popup_risk_blocking=true external_chrome_popup_risk_scope=external_suppression_required risk_count={} external_chrome_popup_risk={} remediation=let the installed Synapse Chrome Bridge management fallback disable the named external debugger/nativeMessaging extensions, or repair HKCU\\Software\\Policies\\Google\\Chrome ACL and rerun scripts\\install-synapse-chrome-debugger.ps1 so ExtensionSettings blocks debugger/nativeMessaging; normal bridge commands fail closed while this risk remains unsuppressed",
        rows.len(),
        format_external_chrome_popup_risks(rows)
    )
}

fn external_chrome_popup_risk_host_unavailable_warning(rows: &[String]) -> String {
    if rows.is_empty() {
        return "external_chrome_popup_risk_warning=false risk_count=0".to_owned();
    }
    format!(
        "external_chrome_popup_risk_warning=true external_chrome_popup_risk_scope=host_unavailable_no_live_management risk_count={} external_chrome_popup_risk={} remediation=the active Synapse Chrome Bridge host is absent, so live Chrome management suppression state cannot be read; restore the already-open authenticated Chrome bridge host with cdp_bridge_reload or the installed bridge reconnect path, then re-read health before classifying external debugger/nativeMessaging rows as suppressed or blocking",
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

fn active_host_available() -> bool {
    let Ok(inner) = bridge().inner.lock() else {
        return false;
    };
    inner
        .active_host_id
        .as_ref()
        .is_some_and(|host_id| inner.hosts.contains_key(host_id))
}

fn active_host_popup_risk_suppression_covers(profile_risk_count: usize) -> bool {
    let Ok(inner) = bridge().inner.lock() else {
        return false;
    };
    inner
        .active_host_id
        .as_ref()
        .and_then(|host_id| inner.hosts.get(host_id))
        .is_some_and(|host| {
            popup_risk_suppression_covers_profile_risks(
                host.extension_popup_risk_suppression.as_ref(),
                profile_risk_count,
            )
        })
}

fn active_host_popup_risk_suppression_summary() -> String {
    let Ok(inner) = bridge().inner.lock() else {
        return "state_lock_poisoned".to_owned();
    };
    inner
        .active_host_id
        .as_ref()
        .and_then(|host_id| inner.hosts.get(host_id))
        .map(|host| popup_risk_suppression_summary(&host.extension_popup_risk_suppression))
        .unwrap_or_else(|| "not_reported".to_owned())
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
                "normal Synapse Chrome Bridge refused command {command_kind:?} before queueing any Chrome tabs/scripting command; hwnd={hwnd} reason=Synapse Chrome Bridge profile still exposes active nativeMessaging permission synapse_chrome_bridge_permission_risk={} remediation=rerun scripts\\install-synapse-chrome-debugger.ps1 so the HKCU ExtensionSettings self-shield blocks nativeMessaging for the Synapse extension ID, then reload the existing bridge through cdp_bridge_reload or keep commands failed closed until Chrome reloads it",
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
            "normal Chrome bridge continuing because Synapse self nativeMessaging residue is granted-only and the live bridge identity/cdpInput capability is verified before commands are queued"
        );
    }
    let risks = external_chrome_popup_risks();
    if risks.is_empty() {
        return Ok(());
    }
    if !active_host_available() {
        tracing::warn!(
            code = "CHROME_EXTERNAL_POPUP_RISK_STATE_UNAVAILABLE",
            hwnd,
            command_kind,
            risk_count = risks.len(),
            external_chrome_popup_risk = %format_external_chrome_popup_risks(&risks),
            "normal Chrome bridge command cannot classify external popup risk because no active bridge host is registered"
        );
        return Err(ChromeDebuggerBridgeError::unavailable());
    }
    let suppression_summary = active_host_popup_risk_suppression_summary();
    if active_host_popup_risk_suppression_covers(risks.len()) {
        tracing::warn!(
            code = "CHROME_EXTERNAL_POPUP_RISK_SUPPRESSED_BY_BRIDGE",
            hwnd,
            command_kind,
            risk_count = risks.len(),
            external_chrome_popup_risk = %format_external_chrome_popup_risks(&risks),
            bridge_popup_risk_suppression = %suppression_summary,
            "normal Chrome bridge continuing because Chrome management readback reported external popup risks suppressed"
        );
        return Ok(());
    }
    tracing::error!(
        code = "CHROME_EXTERNAL_POPUP_RISK_WARNING",
        hwnd,
        command_kind,
        risk_count = risks.len(),
        external_chrome_popup_risk = %format_external_chrome_popup_risks(&risks),
        bridge_popup_risk_suppression = %suppression_summary,
        "normal Chrome bridge refusing tabs/scripting command while external debugger/nativeMessaging risk remains unsuppressed"
    );
    Err(ChromeDebuggerBridgeError {
        code: error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
        detail: format!(
            "normal Synapse Chrome Bridge refused command {command_kind:?} before queueing any Chrome tabs/scripting command; hwnd={hwnd} reason=external debugger/nativeMessaging popup risk remains unsuppressed external_chrome_popup_risk={} bridge_popup_risk_suppression={} remediation=let the installed Synapse Chrome Bridge management fallback disable the named extension IDs, disable them in Chrome, or repair HKCU\\Software\\Policies\\Google\\Chrome ACL so scripts\\install-synapse-chrome-debugger.ps1 can apply ExtensionSettings blocked_permissions for debugger/nativeMessaging",
            format_external_chrome_popup_risks(&risks),
            suppression_summary
        ),
    })
}

fn note_normal_bridge_registration_external_popup_risk() {
    let risks = external_chrome_popup_risks();
    if risks.is_empty() {
        return;
    }
    tracing::warn!(
        code = "CHROME_EXTERNAL_POPUP_RISK_WARNING",
        risk_count = risks.len(),
        external_chrome_popup_risk = %format_external_chrome_popup_risks(&risks),
        "normal Chrome bridge accepting direct registration so the extension can report management suppression state for external debugger/nativeMessaging risk"
    );
}

fn popup_risk_suppression_covers_profile_risks(
    value: Option<&Value>,
    profile_risk_count: usize,
) -> bool {
    let Some(value) = value else {
        return false;
    };
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    if value
        .get("remaining_hazard_count")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        != 0
    {
        return false;
    }
    if profile_risk_count == 0 {
        return true;
    }
    if value
        .get("failure_count")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        != 0
    {
        return false;
    }
    if value.get("management_available").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    matches!(
        value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
        "clear" | "suppressed"
    )
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

fn external_chrome_profile_surfaces() -> Vec<String> {
    let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") else {
        return Vec::new();
    };
    let user_data_root = PathBuf::from(local_appdata)
        .join("Google")
        .join("Chrome")
        .join("User Data");
    let Ok(profile_dirs) = std::fs::read_dir(user_data_root) else {
        return Vec::new();
    };

    let mut rows = Vec::new();
    for profile_dir in profile_dirs.flatten() {
        let Ok(file_type) = profile_dir.file_type() else {
            continue;
        };
        if !file_type.is_dir() || profile_dir.file_name() == "Snapshots" {
            continue;
        }
        let profile = profile_dir.file_name().to_string_lossy().into_owned();
        let mut runtime_by_id: HashMap<String, ChromeExtensionRuntimeState> = HashMap::new();
        for pref_file in ["Preferences", "Secure Preferences"] {
            let pref_path = profile_dir.path().join(pref_file);
            let Ok(raw) = std::fs::read_to_string(&pref_path) else {
                continue;
            };
            let Ok(pref) = serde_json::from_str::<Value>(&raw) else {
                rows.push(format!(
                    "profile={profile} pref={pref_file} parse_error=true"
                ));
                continue;
            };
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
                rows.push(format!(
                    "profile={profile} pref={pref_file} extension_id={extension_id} name={name:?} active_api={} manifest_api={} granted_hazard_api={} popup_risk=true risk_basis={risk_basis} state={} active_bit={} disable_reasons={}",
                    active_permissions.join(","),
                    manifest_permissions.join(","),
                    granted_hazards.join(","),
                    runtime_state
                        .state
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "<absent>".to_owned()),
                    runtime_state
                        .active_bit
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "<absent>".to_owned()),
                    format_disable_reasons(&runtime_state.disable_reasons)
                ));
            }
        }
    }
    rows
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
        .filter(|permission| *permission == "nativeMessaging")
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
                .parent()
                .map(|parent| parent.as_u32().to_string())
                .unwrap_or_else(|| "<unknown>".to_owned());
            let user_data_dir = process_switch_arg_value(&command_args, "--user-data-dir");
            let user_data_dir_state = chrome_user_data_dir_state_label(user_data_dir.as_deref());
            let user_data_dir_display = user_data_dir
                .as_deref()
                .map(quote_detail_value)
                .unwrap_or_else(|| "<missing>".to_owned());
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

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum ChromeDebuggerMouseButton {
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
pub(crate) struct ChromeDebuggerClickPoint {
    pub x: f64,
    pub y: f64,
    pub target_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerTypeResult {
    pub x: f64,
    pub y: f64,
    pub target_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerTypeActiveElementResult {
    pub target_id: String,
    pub tab_id: u32,
    pub chars_typed: u32,
    pub readback_backend: String,
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
pub(crate) struct ChromeDebuggerSetFieldValueResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chars_requested: u32,
    pub readback_backend: String,
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
pub(crate) struct ChromeDebuggerPageContentResult {
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
pub(crate) struct ChromeDebuggerSetContentResult {
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
pub(crate) struct ChromeDebuggerAriaSnapshotNode {
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerAriaSnapshotResult {
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
pub(crate) struct ChromeDebuggerAssertElementState {
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
pub(crate) struct ChromeDebuggerAssertPollResult {
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
pub(crate) struct ChromeDebuggerLocateElementsResult {
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
pub(crate) struct ChromeDebuggerWaitForTextResult {
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
pub(crate) struct ChromeDebuggerWaitForFunctionResult {
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
    pub expression_len: usize,
    #[serde(default)]
    pub arg_count: usize,
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub value_type: String,
    #[serde(default)]
    pub value_description: Option<String>,
    #[serde(default)]
    pub unserializable_value: Option<String>,
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
pub(crate) struct ChromeDebuggerWaitForLoadStateResult {
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
pub(crate) struct ChromeDebuggerWaitForUrlResult {
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
pub(crate) struct ChromeDebuggerNetworkWaitEntry {
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
pub(crate) struct ChromeDebuggerWaitForNetworkResult {
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
pub(crate) struct ChromeDebuggerWaitForSelectorResult {
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
pub(crate) struct ChromeDebuggerInspectElementResult {
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
pub(crate) struct ChromeDebuggerScrollIntoViewResult {
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
pub(crate) struct ChromeDebuggerClockReadback {
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
pub(crate) struct ChromeDebuggerClockResult {
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
pub(crate) struct ChromeDebuggerPageEventEntry {
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
pub(crate) struct ChromeDebuggerPageTargetSnapshot {
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
pub(crate) struct ChromeDebuggerWorkerSnapshot {
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
pub(crate) struct ChromeDebuggerPageEventsFilters {
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
pub(crate) struct ChromeDebuggerPageEventsResult {
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
pub(crate) struct ChromeDebuggerNodeValue {
    pub value: String,
    pub target_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerOpenTabResult {
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
pub(crate) struct ChromeDebuggerCloseTabResult {
    pub target_id: String,
    pub tab_id: u32,
    pub target_count_before: u32,
    pub target_count_after: u32,
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerTabTarget {
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
pub(crate) struct ChromeDebuggerListTabsResult {
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
pub(crate) struct ChromeDebuggerTargetInfo {
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
pub(crate) struct ChromeDebuggerViewportOverride {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
    pub mobile: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerViewportReadback {
    pub inner_width: i64,
    pub inner_height: i64,
    pub device_pixel_ratio: f64,
    pub screen_width: i64,
    pub screen_height: i64,
    pub outer_width: i64,
    pub outer_height: i64,
    #[serde(default)]
    pub visual_viewport_width: Option<f64>,
    #[serde(default)]
    pub visual_viewport_height: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerViewportEmulationResult {
    pub extension_id: Option<String>,
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub operation: String,
    #[serde(default)]
    pub requested: Option<ChromeDebuggerViewportOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub viewport: ChromeDebuggerViewportReadback,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    #[serde(default)]
    pub source_of_truth: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub debugger_protocol_version: Option<String>,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerDeviceDescriptor {
    pub user_agent: String,
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
    pub is_mobile: bool,
    pub has_touch: bool,
    pub max_touch_points: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerDeviceReadback {
    pub viewport: ChromeDebuggerViewportReadback,
    pub user_agent: String,
    pub max_touch_points: i64,
    pub ontouchstart_available: bool,
    pub pointer_coarse: bool,
    pub any_pointer_coarse: bool,
    pub hover_none: bool,
    pub any_hover_none: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerDeviceEmulationResult {
    pub extension_id: Option<String>,
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub operation: String,
    #[serde(default)]
    pub descriptor: Option<ChromeDebuggerDeviceDescriptor>,
    #[serde(default)]
    pub restored_user_agent: Option<String>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub device: ChromeDebuggerDeviceReadback,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    #[serde(default)]
    pub source_of_truth: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub debugger_protocol_version: Option<String>,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerGeolocationOverride {
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: f64,
    #[serde(default)]
    pub altitude: Option<f64>,
    #[serde(default)]
    pub altitude_accuracy: Option<f64>,
    #[serde(default)]
    pub heading: Option<f64>,
    #[serde(default)]
    pub speed: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerGeolocationCoordinatesReadback {
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: f64,
    #[serde(default)]
    pub altitude: Option<f64>,
    #[serde(default)]
    pub altitude_accuracy: Option<f64>,
    #[serde(default)]
    pub heading: Option<f64>,
    #[serde(default)]
    pub speed: Option<f64>,
    pub timestamp: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerGeolocationErrorReadback {
    pub code: i64,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerGeolocationReadback {
    pub permission_state: String,
    #[serde(default)]
    pub position: Option<ChromeDebuggerGeolocationCoordinatesReadback>,
    #[serde(default)]
    pub error: Option<ChromeDebuggerGeolocationErrorReadback>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerGeolocationEmulationResult {
    pub extension_id: Option<String>,
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub operation: String,
    pub origin: String,
    #[serde(default)]
    pub requested: Option<ChromeDebuggerGeolocationOverride>,
    pub permission_setting: String,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub geolocation: ChromeDebuggerGeolocationReadback,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    #[serde(default)]
    pub source_of_truth: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub debugger_protocol_version: Option<String>,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerLocaleTimezoneOverride {
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub timezone_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerLocaleTimezoneReadback {
    pub locale: String,
    pub calendar: String,
    pub numbering_system: String,
    pub time_zone: String,
    pub sample_number: String,
    pub sample_date: String,
    pub date_string: String,
    pub timezone_offset_minutes: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerLocaleEmulationResult {
    pub extension_id: Option<String>,
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub operation: String,
    #[serde(default)]
    pub requested: Option<ChromeDebuggerLocaleTimezoneOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub locale: ChromeDebuggerLocaleTimezoneReadback,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    #[serde(default)]
    pub source_of_truth: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub debugger_protocol_version: Option<String>,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerMediaOverride {
    #[serde(default)]
    pub media: Option<String>,
    #[serde(default)]
    pub color_scheme: Option<String>,
    #[serde(default)]
    pub reduced_motion: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerMediaReadback {
    pub media_screen: bool,
    pub media_print: bool,
    pub color_scheme_dark: bool,
    pub color_scheme_light: bool,
    pub color_scheme_no_preference: bool,
    pub reduced_motion_reduce: bool,
    pub reduced_motion_no_preference: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerMediaEmulationResult {
    pub extension_id: Option<String>,
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub operation: String,
    #[serde(default)]
    pub requested: Option<ChromeDebuggerMediaOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub media: ChromeDebuggerMediaReadback,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    #[serde(default)]
    pub source_of_truth: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub debugger_protocol_version: Option<String>,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerNetworkConditionsOverride {
    pub offline: bool,
    pub latency_ms: f64,
    pub download_throughput_bytes_per_sec: f64,
    pub upload_throughput_bytes_per_sec: f64,
    #[serde(default)]
    pub connection_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerNetworkConditionsReadback {
    pub online: bool,
    #[serde(default)]
    pub connection_type: Option<String>,
    #[serde(default)]
    pub effective_type: Option<String>,
    #[serde(default)]
    pub downlink_mbps: Option<f64>,
    #[serde(default)]
    pub rtt_ms: Option<f64>,
    #[serde(default)]
    pub save_data: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerNetworkConditionsResult {
    pub extension_id: Option<String>,
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub operation: String,
    #[serde(default)]
    pub requested: Option<ChromeDebuggerNetworkConditionsOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub network: ChromeDebuggerNetworkConditionsReadback,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub required_foreground: bool,
    #[serde(default)]
    pub source_of_truth: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub debugger_protocol_version: Option<String>,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerFrameEntry {
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
pub(crate) struct ChromeDebuggerLocatedFrame {
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
pub(crate) struct ChromeDebuggerFramesResult {
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
pub(crate) struct ChromeDebuggerCaptureVisibleTabResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub chrome_window_focused: Option<bool>,
    #[serde(default)]
    pub chrome_window_state: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub before_active: bool,
    #[serde(default)]
    pub active_for_capture: bool,
    #[serde(default)]
    pub before_highlighted: bool,
    #[serde(default)]
    pub highlighted_for_capture: bool,
    #[serde(default)]
    pub previous_active_tab_id: Option<u32>,
    #[serde(default)]
    pub restored_previous_active: bool,
    #[serde(default)]
    pub image_format: String,
    pub image_data_url: String,
    #[serde(default)]
    pub image_data_url_len: usize,
    #[serde(default)]
    pub capture_attempt_count: usize,
    #[serde(default)]
    pub capture_attempts: Vec<ChromeDebuggerCaptureAttempt>,
    #[serde(default)]
    pub readback_backend: String,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerPageScreenshotResult {
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
    pub before_active: bool,
    #[serde(default)]
    pub active_for_capture: bool,
    #[serde(default)]
    pub previous_active_tab_id: Option<u32>,
    #[serde(default)]
    pub restored_previous_active: bool,
    #[serde(default)]
    pub image_format: String,
    #[serde(default)]
    pub quality: Option<u8>,
    #[serde(default)]
    pub omit_background: bool,
    #[serde(default)]
    pub scope: String,
    pub clip_css: ChromeDebuggerPageScreenshotRect,
    #[serde(default)]
    pub output_css_width: f64,
    #[serde(default)]
    pub output_css_height: f64,
    #[serde(default)]
    pub device_pixel_ratio: f64,
    #[serde(default)]
    pub scroll_width_css: f64,
    #[serde(default)]
    pub scroll_height_css: f64,
    #[serde(default)]
    pub viewport_width_css: f64,
    #[serde(default)]
    pub viewport_height_css: f64,
    #[serde(default)]
    pub tile_count: usize,
    #[serde(default)]
    pub tiles: Vec<ChromeDebuggerPageScreenshotTile>,
    #[serde(default)]
    pub capture_attempt_count: usize,
    #[serde(default)]
    pub capture_attempts: Vec<ChromeDebuggerCaptureAttempt>,
    #[serde(default)]
    pub mask_count: usize,
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
pub(crate) struct ChromeDebuggerPagePdfResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    pub data_base64: String,
    #[serde(default)]
    pub data_base64_len: usize,
    #[serde(default)]
    pub pdf_byte_length: usize,
    #[serde(default)]
    pub landscape: bool,
    #[serde(default)]
    pub print_background: bool,
    #[serde(default)]
    pub display_header_footer: bool,
    #[serde(default)]
    pub scale: f64,
    #[serde(default)]
    pub paper_width: f64,
    #[serde(default)]
    pub paper_height: f64,
    #[serde(default)]
    pub margin_top: f64,
    #[serde(default)]
    pub margin_bottom: f64,
    #[serde(default)]
    pub margin_left: f64,
    #[serde(default)]
    pub margin_right: f64,
    #[serde(default)]
    pub page_ranges: String,
    #[serde(default)]
    pub prefer_css_page_size: bool,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub backend_tier_used: String,
    #[serde(default)]
    pub protocol_version: String,
    #[serde(default)]
    pub target_candidate_count: u32,
    #[serde(default)]
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerDownloadsResult {
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
pub(crate) struct ChromeDebuggerDownloadEntry {
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
pub(crate) struct ChromeDebuggerDownloadEvent {
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

#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerPageScreenshotRect {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default, alias = "width")]
    pub w: f64,
    #[serde(default, alias = "height")]
    pub h: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerPageScreenshotTile {
    #[serde(default)]
    pub scroll_x_css: f64,
    #[serde(default)]
    pub scroll_y_css: f64,
    #[serde(default)]
    pub viewport_width_css: f64,
    #[serde(default)]
    pub viewport_height_css: f64,
    pub image_data_url: String,
    #[serde(default)]
    pub image_data_url_len: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerCaptureAttempt {
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default, deserialize_with = "deserialize_null_default_string")]
    pub error_detail: String,
    #[serde(default)]
    pub retryable: bool,
}

fn deserialize_null_default_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerLargestContentfulPaint {
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
pub(crate) struct ChromeDebuggerPageVitals {
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
pub(crate) struct ChromeDebuggerPageText {
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
pub(crate) struct ChromeDebuggerFrameReadback {
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
pub(crate) struct ChromeDebuggerActiveElement {
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
pub(crate) struct ChromeDebuggerNavigateResult {
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
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerActivateTabResult {
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
pub(crate) struct ChromeDebuggerEvaluateScriptResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub result_type: String,
    #[serde(default)]
    pub result_subtype: Option<String>,
    #[serde(default)]
    pub returned_by_value: bool,
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub unserializable_value: Option<String>,
    #[serde(default)]
    pub readback_backend: String,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerInitScriptResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub operation: String,
    pub identifier: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub readback_backend: String,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerBindingCall {
    pub seq: u64,
    pub name: String,
    pub payload: String,
    pub payload_len: usize,
    pub payload_truncated: bool,
    #[serde(default)]
    pub payload_json: Option<Value>,
    pub execution_context_id: i64,
    pub timestamp_ms: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerExposeBindingResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub operation: String,
    pub name: String,
    #[serde(default)]
    pub newly_armed: bool,
    #[serde(default)]
    pub binding_newly_added: bool,
    #[serde(default)]
    pub binding_removed: bool,
    #[serde(default)]
    pub armed_at_unix_ms: f64,
    #[serde(default)]
    pub binding_active: bool,
    #[serde(default)]
    pub active_binding_count: usize,
    #[serde(default)]
    pub active_binding_names: Vec<String>,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub calls: Vec<ChromeDebuggerBindingCall>,
    #[serde(default)]
    pub next_cursor: u64,
    #[serde(default)]
    pub returned: usize,
    #[serde(default)]
    pub total_buffered: usize,
    #[serde(default)]
    pub dropped: u64,
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
pub(crate) struct ChromeDebuggerDialogEntry {
    pub seq: u64,
    pub url: String,
    pub frame_id: String,
    pub dialog_type: String,
    pub message: String,
    #[serde(default)]
    pub default_prompt: Option<String>,
    #[serde(default)]
    pub has_browser_handler: bool,
    pub opened_at_unix_ms: u64,
    pub pending: bool,
    pub default_policy: String,
    #[serde(default)]
    pub auto_action: Option<String>,
    #[serde(default)]
    pub auto_handled_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub auto_handle_error: Option<String>,
    #[serde(default)]
    pub manual_action: Option<String>,
    #[serde(default)]
    pub manual_prompt_text: Option<String>,
    #[serde(default)]
    pub manual_handled_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub manual_handle_error: Option<String>,
    #[serde(default)]
    pub closed_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub close_result: Option<bool>,
    #[serde(default)]
    pub user_input: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerHandleDialogResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub operation: String,
    pub default_policy: String,
    #[serde(default)]
    pub capture_newly_armed: bool,
    #[serde(default)]
    pub handled: bool,
    #[serde(default)]
    pub handle_action: Option<String>,
    #[serde(default)]
    pub prompt_text: Option<String>,
    #[serde(default)]
    pub pending_dialog: Option<ChromeDebuggerDialogEntry>,
    #[serde(default)]
    pub handled_dialog: Option<ChromeDebuggerDialogEntry>,
    #[serde(default)]
    pub last_dialog: Option<ChromeDebuggerDialogEntry>,
    #[serde(default)]
    pub entries: Vec<ChromeDebuggerDialogEntry>,
    #[serde(default)]
    pub next_cursor: u64,
    #[serde(default)]
    pub returned: usize,
    #[serde(default)]
    pub total_buffered: usize,
    #[serde(default)]
    pub dropped: u64,
    #[serde(default)]
    pub opened_count: u64,
    #[serde(default)]
    pub closed_count: u64,
    #[serde(default)]
    pub auto_handled_count: u64,
    #[serde(default)]
    pub error_count: u64,
    pub url: String,
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
pub(crate) struct ChromeDebuggerFileUploadFile {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub last_modified: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerFileUploadInput {
    #[serde(default)]
    pub resolved_by: String,
    #[serde(default)]
    pub match_count: u32,
    #[serde(default)]
    pub frame_id: Option<i64>,
    #[serde(default)]
    pub element_path: Option<String>,
    #[serde(default)]
    pub backend_node_id: Option<i64>,
    #[serde(default)]
    pub tag_name: String,
    #[serde(default)]
    pub type_attr: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name_attr: String,
    #[serde(default)]
    pub accept: String,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default)]
    pub webkitdirectory: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub file_count: usize,
    #[serde(default)]
    pub files: Vec<ChromeDebuggerFileUploadFile>,
    #[serde(default)]
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerFileChooserEntry {
    pub seq: u64,
    #[serde(default)]
    pub frame_id: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub backend_node_id: Option<i64>,
    pub opened_at_unix_ms: u64,
    #[serde(default)]
    pub pending: bool,
    #[serde(default)]
    pub handled_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub canceled_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub requested_file_count: Option<usize>,
    #[serde(default)]
    pub file_names: Vec<String>,
    #[serde(default)]
    pub input: Option<ChromeDebuggerFileUploadInput>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeDebuggerFileUploadResult {
    pub target_id: String,
    pub tab_id: u32,
    #[serde(default)]
    pub chrome_window_id: Option<i64>,
    pub operation: String,
    #[serde(default)]
    pub capture_newly_armed: bool,
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub element_id: Option<String>,
    #[serde(default)]
    pub active_element: bool,
    #[serde(default)]
    pub requested_file_count: usize,
    #[serde(default)]
    pub input: Option<ChromeDebuggerFileUploadInput>,
    #[serde(default)]
    pub handled_chooser: Option<ChromeDebuggerFileChooserEntry>,
    #[serde(default)]
    pub canceled_chooser: Option<ChromeDebuggerFileChooserEntry>,
    #[serde(default)]
    pub pending_chooser: Option<ChromeDebuggerFileChooserEntry>,
    #[serde(default)]
    pub entries: Vec<ChromeDebuggerFileChooserEntry>,
    #[serde(default)]
    pub next_cursor: u64,
    #[serde(default)]
    pub returned: usize,
    #[serde(default)]
    pub total_buffered: usize,
    #[serde(default)]
    pub dropped: u64,
    #[serde(default)]
    pub opened_count: u64,
    #[serde(default)]
    pub handled_count: u64,
    #[serde(default)]
    pub canceled_count: u64,
    #[serde(default)]
    pub error_count: u64,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub ready_state: String,
    #[serde(default)]
    pub readback_backend: String,
    #[serde(default)]
    pub chooser_readback_backend: String,
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
pub(crate) struct ChromeBridgeReloadCommandAck {
    pub ok: bool,
    #[serde(rename = "extensionId")]
    pub extension_id: String,
    pub version: String,
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    #[serde(rename = "buildId")]
    pub build_id: String,
    #[serde(rename = "buildSha256")]
    pub build_sha256: String,
    #[serde(default, rename = "debuggerApiAvailable")]
    pub debugger_api_available: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub host_id: Option<String>,
    pub reload_requested_at_unix_ms: u64,
    pub reload_delay_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChromeBridgeHostSnapshot {
    pub host_id: String,
    pub origin: String,
    pub extension_id: Option<String>,
    pub extension_version: Option<String>,
    pub extension_protocol_version: Option<u32>,
    pub extension_build_id: Option<String>,
    pub extension_build_sha256: Option<String>,
    pub extension_capabilities: Vec<String>,
    pub extension_user_agent: Option<String>,
    pub extension_debugger_api_available: Option<bool>,
    pub extension_popup_risk_suppression: Option<Value>,
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
pub(crate) struct ChromeBridgeReloadResult {
    pub before: ChromeBridgeHostSnapshot,
    pub command_ack: ChromeBridgeReloadCommandAck,
    pub after: ChromeBridgeHostSnapshot,
    pub reconnected: bool,
    pub waited_ms: u64,
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
pub(crate) struct NativeRegisterRequest {
    origin: String,
    pid: u32,
    parent_window: Option<String>,
    bridge_protocol_version: u32,
    transport: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct NativeMessageRequest {
    host_id: String,
    message: Value,
}

#[derive(Debug, Deserialize)]
pub(crate) struct NativeNextQuery {
    host_id: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct NativeWsQuery {
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

#[derive(Debug, Deserialize)]
struct ChromeResponse {
    id: String,
    ok: bool,
    result: Option<Value>,
    error: Option<ChromeResponseError>,
}

#[derive(Debug, Deserialize)]
struct ChromeResponseError {
    code: Option<String>,
    detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ExtensionTabNavigationEvent {
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
    observed_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct ChromeDebuggerBrowserNavigationEvent {
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
}

struct PendingResponse {
    host_id: String,
    kind: String,
    sender: oneshot::Sender<ChromeResponse>,
}

#[derive(Clone, Debug)]
struct HostRecord {
    origin: String,
    extension_id: Option<String>,
    extension_version: Option<String>,
    extension_protocol_version: Option<u32>,
    extension_build_id: Option<String>,
    extension_build_sha256: Option<String>,
    extension_capabilities: BTreeSet<String>,
    extension_user_agent: Option<String>,
    extension_debugger_api_available: Option<bool>,
    extension_popup_risk_suppression: Option<Value>,
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
    extension_capabilities: BTreeSet<String>,
    extension_user_agent: Option<String>,
    extension_debugger_api_available: Option<bool>,
    extension_popup_risk_suppression: Option<Value>,
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
    hosts: HashMap<String, HostRecord>,
    commands: VecDeque<QueuedCommand>,
    pending: HashMap<String, PendingResponse>,
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

pub(crate) fn set_browser_navigation_sink(sink: Arc<BrowserNavigationSink>) {
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
    if host.transport.as_deref() != Some("direct_http") {
        return None;
    }
    if host.extension_id.as_deref() != Some(EXTENSION_ID) {
        return Some(format!(
            "extension_id_mismatch actual={} expected={EXTENSION_ID}",
            host.extension_id.as_deref().unwrap_or("not_seen_yet")
        ));
    }
    let mut identity_reasons = Vec::new();
    if host.extension_build_id.as_deref() != Some(EXPECTED_EXTENSION_BUILD_ID) {
        identity_reasons.push(format!(
            "build_id={} expected={EXPECTED_EXTENSION_BUILD_ID}",
            host.extension_build_id.as_deref().unwrap_or("not_seen_yet")
        ));
    }
    if host.extension_build_sha256.as_deref() != Some(EXPECTED_EXTENSION_BUILD_SHA256) {
        identity_reasons.push(format!(
            "build_sha256={} expected={EXPECTED_EXTENSION_BUILD_SHA256}",
            host.extension_build_sha256
                .as_deref()
                .unwrap_or("not_seen_yet")
        ));
    }
    if kind == "reloadSelf" {
        if host.extension_capabilities.contains(kind) {
            return None;
        }
        return Some(format!(
            "missing_capability=reloadSelf loaded_capabilities={}",
            format_capabilities(&host.extension_capabilities)
        ));
    }
    if !identity_reasons.is_empty() {
        return Some(identity_reasons.join("|"));
    }
    if host.extension_debugger_api_available != Some(true) {
        return Some(format!(
            "debugger_api_available={} expected=true",
            host.extension_debugger_api_available
                .map(|value| value.to_string())
                .unwrap_or_else(|| "not_seen_yet".to_owned())
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

fn bridge_identity_stale_reasons(host: &ChromeBridgeHealthRecord) -> Vec<String> {
    let mut reasons = Vec::new();
    if host.extension_id.as_deref() != Some(EXTENSION_ID) {
        reasons.push(format!(
            "extension_id={} expected={EXTENSION_ID}",
            host.extension_id.as_deref().unwrap_or("not_seen_yet")
        ));
    }
    if host.extension_build_id.as_deref() != Some(EXPECTED_EXTENSION_BUILD_ID) {
        reasons.push(format!(
            "build_id={} expected={EXPECTED_EXTENSION_BUILD_ID}",
            host.extension_build_id.as_deref().unwrap_or("not_seen_yet")
        ));
    }
    if host.extension_build_sha256.as_deref() != Some(EXPECTED_EXTENSION_BUILD_SHA256) {
        reasons.push(format!(
            "build_sha256={} expected={EXPECTED_EXTENSION_BUILD_SHA256}",
            host.extension_build_sha256
                .as_deref()
                .unwrap_or("not_seen_yet")
        ));
    }
    if host.extension_debugger_api_available != Some(true) {
        reasons.push(format!(
            "debugger_api_available={} expected=true",
            host.extension_debugger_api_available
                .map(|value| value.to_string())
                .unwrap_or_else(|| "not_seen_yet".to_owned())
        ));
    }
    let missing = bridge_missing_required_capabilities(&host.extension_capabilities);
    if !missing.is_empty() {
        reasons.push(format!("missing_capabilities={}", missing.join(",")));
    }
    reasons
}

fn host_record_to_health_record(host_id: &str, host: &HostRecord) -> ChromeBridgeHealthRecord {
    ChromeBridgeHealthRecord {
        host_id: host_id.to_owned(),
        origin: host.origin.clone(),
        extension_id: host.extension_id.clone(),
        extension_version: host.extension_version.clone(),
        extension_protocol_version: host.extension_protocol_version,
        extension_build_id: host.extension_build_id.clone(),
        extension_build_sha256: host.extension_build_sha256.clone(),
        extension_capabilities: host.extension_capabilities.clone(),
        extension_user_agent: host.extension_user_agent.clone(),
        extension_debugger_api_available: host.extension_debugger_api_available,
        extension_popup_risk_suppression: host.extension_popup_risk_suppression.clone(),
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
        extension_capabilities: host.extension_capabilities.iter().cloned().collect(),
        extension_user_agent: host.extension_user_agent.clone(),
        extension_debugger_api_available: host.extension_debugger_api_available,
        extension_popup_risk_suppression: host.extension_popup_risk_suppression.clone(),
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

fn reload_self_expected_loaded_build_id(snapshot: &ChromeBridgeHostSnapshot) -> Option<&str> {
    snapshot
        .extension_build_id
        .as_deref()
        .filter(|build_id| !build_id.trim().is_empty())
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
            extension_capabilities: BTreeSet::new(),
            extension_user_agent: None,
            extension_debugger_api_available: None,
            extension_popup_risk_suppression: None,
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
                let popup_risk_suppression =
                    popup_risk_suppression_summary(&host.extension_popup_risk_suppression);
                tracing::info!(
                    code = "CHROME_DEBUGGER_EXTENSION_HELLO",
                    host_id = %request.host_id,
                    origin = %host.origin,
                    extension_id = host.extension_id.as_deref().unwrap_or_default(),
                    extension_version = host.extension_version.as_deref().unwrap_or_default(),
                    extension_protocol_version = host.extension_protocol_version.unwrap_or_default(),
                    extension_build_id = host.extension_build_id.as_deref().unwrap_or_default(),
                    extension_build_sha256 = host.extension_build_sha256.as_deref().unwrap_or_default(),
                    debugger_api_available = host.extension_debugger_api_available.unwrap_or(true),
                    capabilities = %format_capabilities(&host.extension_capabilities),
                    popup_risk_suppression = %popup_risk_suppression,
                    pid = host.pid,
                    parent_window = host.parent_window.as_deref().unwrap_or_default(),
                    transport = host.transport.as_deref().unwrap_or("native_messaging"),
                    registered_unix_ms = host.registered_unix_ms,
                    "Chrome debugger extension connected through native host"
                );
            }
            "response" => {
                let response = serde_json::from_value::<ChromeResponse>(request.message)
                    .map_err(|error| format!("decode chrome debugger response: {error}"))?;
                let id = response.id.clone();
                if inner
                    .pending
                    .get(&id)
                    .is_some_and(|pending| pending.host_id != request.host_id)
                {
                    tracing::warn!(
                        code = "CHROME_DEBUGGER_RESPONSE_HOST_MISMATCH",
                        host_id = %request.host_id,
                        command_id = %id,
                        "Chrome debugger response came from a different host than the pending command owner"
                    );
                    return Ok(());
                }
                let Some(pending) = inner.pending.remove(&id) else {
                    tracing::warn!(
                        code = "CHROME_DEBUGGER_RESPONSE_WITHOUT_PENDING_COMMAND",
                        host_id = %request.host_id,
                        command_id = %id,
                        "Chrome debugger response had no pending daemon command"
                    );
                    return Ok(());
                };
                let readback_summary =
                    chrome_response_readback_summary(&pending.kind, response.result.as_ref());
                tracing::info!(
                    code = "CHROME_DEBUGGER_RESPONSE_ACCEPTED",
                    host_id = %request.host_id,
                    command_id = %id,
                    command_kind = %pending.kind,
                    response_ok = response.ok,
                    readback = %readback_summary.as_deref().unwrap_or(""),
                    "Chrome debugger response accepted"
                );
                let _ = pending.sender.send(response);
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
                    }
                    inner
                        .commands
                        .retain(|queued| queued.host_id != request.host_id);
                    let pending_ids = inner
                        .pending
                        .iter()
                        .filter_map(|(id, pending)| {
                            if pending.host_id == request.host_id {
                                Some(id.clone())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>();
                    for id in pending_ids {
                        if let Some(pending) = inner.pending.remove(&id) {
                            let _ = pending.sender.send(ChromeResponse {
                                id,
                                ok: false,
                                result: None,
                                error: Some(ChromeResponseError {
                                    code: Some(
                                        error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE.to_owned(),
                                    ),
                                    detail: Some(detail.clone().unwrap_or_else(|| {
                                        "Chrome debugger native port disconnected".to_owned()
                                    })),
                                }),
                            });
                        }
                    }
                    tracing::warn!(
                        code = "CHROME_DEBUGGER_NATIVE_PORT_DISCONNECTED",
                        host_id = %request.host_id,
                        detail = %detail_for_log,
                        "Chrome debugger native port disconnected"
                    );
                } else if event == "tabNavigation" {
                    let decoded = serde_json::from_value::<ExtensionTabNavigationEvent>(
                        request.message.clone(),
                    )
                    .map_err(|error| format!("decode Chrome tabNavigation event: {error}"))?;
                    browser_navigation_event = Some(ChromeDebuggerBrowserNavigationEvent {
                        source: decoded.source,
                        event: "tabNavigation".to_owned(),
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
                    });
                    tracing::info!(
                        code = "CHROME_DEBUGGER_BROWSER_NAVIGATION_EVENT",
                        host_id = %request.host_id,
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

    async fn reload_self(
        &self,
        wait_timeout_ms: u64,
    ) -> Result<ChromeBridgeReloadResult, ChromeDebuggerBridgeError> {
        let wait_timeout = Duration::from_millis(wait_timeout_ms);
        let before = self.active_host_snapshot()?;
        let mut params = json!({
            "expectedExtensionId": EXTENSION_ID,
            "reloadDelayMs": 100u64,
        });
        if let Some(loaded_build_id) = reload_self_expected_loaded_build_id(&before) {
            params["expectedBuildId"] = json!(loaded_build_id);
        }
        let ack = self
            .send_command("reloadSelf", params)
            .await
            .and_then(|result| {
                serde_json::from_value::<ChromeBridgeReloadCommandAck>(result).map_err(|error| {
                    ChromeDebuggerBridgeError::protocol(format!(
                        "decode Chrome bridge reloadSelf acknowledgement: {error}"
                    ))
                })
            })?;
        let started = Instant::now();
        loop {
            if started.elapsed() >= wait_timeout {
                return Err(ChromeDebuggerBridgeError::timeout("reloadSelf"));
            }
            if let Ok(after) = self.active_host_snapshot()
                && after.host_id != before.host_id
                && after.extension_id.as_deref() == Some(EXTENSION_ID)
                && after.last_disconnect_detail.is_none()
            {
                let waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                if after.extension_stale {
                    return Err(ChromeDebuggerBridgeError {
                        code: error_codes::CHROME_BRIDGE_EXTENSION_STALE,
                        detail: format!(
                            "Chrome bridge reconnected after reloadSelf but loaded extension is still stale; before_host_id={} after_host_id={} stale_reasons={} waited_ms={waited_ms}",
                            before.host_id,
                            after.host_id,
                            after.extension_stale_reasons.join("|")
                        ),
                    });
                }
                return Ok(ChromeBridgeReloadResult {
                    before,
                    command_ack: ack,
                    after,
                    reconnected: true,
                    waited_ms,
                });
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
                    sender,
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

        let response = match timeout(COMMAND_TIMEOUT, receiver).await {
            Ok(Ok(response)) => response,
            Ok(Err(_closed)) => {
                self.drop_pending(&id);
                return Err(ChromeDebuggerBridgeError::protocol(format!(
                    "Chrome debugger command {kind:?} response channel closed"
                )));
            }
            Err(_elapsed) => {
                self.drop_pending(&id);
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

    fn drop_pending(&self, id: &str) {
        if let Ok(mut inner) = self.inner.lock()
            && let Some(pending) = inner.pending.remove(id)
        {
            tracing::warn!(
                code = "CHROME_DEBUGGER_PENDING_DROPPED",
                command_id = %id,
                command_kind = %pending.kind,
                "Chrome debugger pending command removed"
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
        }
        let queued_before = inner.commands.len();
        inner.commands.retain(|queued| queued.host_id != host_id);
        let queued_removed = queued_before.saturating_sub(inner.commands.len());
        let pending_ids = inner
            .pending
            .iter()
            .filter_map(|(id, pending)| {
                if pending.host_id == host_id {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for id in &pending_ids {
            if let Some(pending) = inner.pending.remove(id) {
                let _ = pending.sender.send(ChromeResponse {
                    id: id.clone(),
                    ok: false,
                    result: None,
                    error: Some(ChromeResponseError {
                        code: Some(error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE.to_owned()),
                        detail: Some(format!(
                            "Chrome debugger direct HTTP bridge host {host_id} disconnected before command response: {detail}"
                        )),
                    }),
                });
            }
        }
        inner.hosts.remove(host_id);
        tracing::warn!(
            code = "CHROME_DEBUGGER_DIRECT_HTTP_WS_DISCONNECTED",
            host_id = %host_id,
            detail = %detail,
            queued_removed,
            pending_failed = pending_ids.len(),
            "Chrome debugger direct HTTP WebSocket disconnected"
        );
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

pub(crate) fn health_subsystem() -> SubsystemHealth {
    let popup_risks = external_chrome_popup_risks();
    let self_profile_risks = synapse_chrome_self_profile_surfaces();
    let layout_infobar_risks = external_chrome_layout_infobar_processes();
    let profile_install_state = synapse_chrome_profile_install_state();
    let snapshot = match bridge().inner.lock() {
        Ok(inner) => {
            let active_host = inner
                .active_host_id
                .as_ref()
                .and_then(|host_id| inner.hosts.get(host_id).map(|host| (host_id, host)))
                .map(|(host_id, host)| ChromeBridgeHealthRecord {
                    host_id: host_id.clone(),
                    origin: host.origin.clone(),
                    extension_id: host.extension_id.clone(),
                    extension_version: host.extension_version.clone(),
                    extension_protocol_version: host.extension_protocol_version,
                    extension_build_id: host.extension_build_id.clone(),
                    extension_build_sha256: host.extension_build_sha256.clone(),
                    extension_capabilities: host.extension_capabilities.clone(),
                    extension_user_agent: host.extension_user_agent.clone(),
                    extension_debugger_api_available: host.extension_debugger_api_available,
                    extension_popup_risk_suppression: host.extension_popup_risk_suppression.clone(),
                    pid: host.pid,
                    parent_window: host.parent_window.clone(),
                    transport: host.transport.clone(),
                    registered_unix_ms: host.registered_unix_ms,
                    last_seen_unix_ms: host.last_seen_unix_ms,
                    last_disconnect_detail: host.last_disconnect_detail.clone(),
                    last_detach_reason: host.last_detach_reason.clone(),
                });
            (
                active_host,
                inner.hosts.len(),
                inner.commands.len(),
                inner.pending.len(),
            )
        }
        Err(_poisoned) => {
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

#[cfg(test)]
fn chrome_bridge_health_from_snapshot(
    active_host: Option<&ChromeBridgeHealthRecord>,
    host_count: usize,
    queued_count: usize,
    pending_count: usize,
    popup_risks: &[String],
    self_profile_risks: &[String],
    layout_infobar_risks: &[String],
) -> SubsystemHealth {
    let self_policy_shield = synapse_chrome_self_policy_shield_status();
    let profile_install_state = SynapseChromeProfileInstallState::not_scanned("snapshot_test");
    chrome_bridge_health_from_snapshot_with_self_policy(
        active_host,
        host_count,
        queued_count,
        pending_count,
        popup_risks,
        self_profile_risks,
        layout_infobar_risks,
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
        .map(|value| value.to_string())
        .unwrap_or_else(|| "not_seen_yet".to_owned());
    let extension_build_id = host.extension_build_id.as_deref().unwrap_or("not_seen_yet");
    let extension_build_sha256 = host
        .extension_build_sha256
        .as_deref()
        .unwrap_or("not_seen_yet");
    let extension_capabilities = format_capabilities(&host.extension_capabilities);
    let extension_user_agent = host.extension_user_agent.as_deref().unwrap_or("");
    let extension_debugger_api_available = host
        .extension_debugger_api_available
        .map(|value| value.to_string())
        .unwrap_or_else(|| "not_seen_yet".to_owned());
    let popup_risk_suppression =
        popup_risk_suppression_summary(&host.extension_popup_risk_suppression);
    let popup_risk_suppression_ok = popup_risk_suppression_covers_profile_risks(
        host.extension_popup_risk_suppression.as_ref(),
        popup_risks.len(),
    );
    let risk_warning = external_chrome_popup_risk_warning(popup_risks, popup_risk_suppression_ok);
    let stale_reasons = bridge_identity_stale_reasons(host);
    let extension_stale = !stale_reasons.is_empty();
    let extension_stale_reasons = if stale_reasons.is_empty() {
        "none".to_owned()
    } else {
        stale_reasons.join("|")
    };
    let popup_risk_blocking = !popup_risks.is_empty() && !popup_risk_suppression_ok;
    let tab_control_available = extension_id == EXTENSION_ID
        && host.last_disconnect_detail.is_none()
        && !extension_stale
        && !self_permission_blocking
        && !popup_risk_blocking;
    let status = if tab_control_available {
        "ok"
    } else if extension_stale {
        "stale"
    } else if self_permission_blocking || popup_risk_blocking {
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
            "tab_control_available={} extension_stale={} extension_stale_reasons={} active_host_id={} host_count={} origin={} extension_id={} expected_extension_id={} extension_version={} extension_protocol_version={} extension_build_id={} expected_extension_build_id={} extension_build_sha256={} expected_extension_build_sha256={} extension_debugger_api_available={} expected_extension_debugger_api_available=true extension_capabilities={} required_extension_capabilities={} endpoint={} transport={} pid={} parent_window={} registered_unix_ms={} last_seen_unix_ms={} queued_count={} pending_count={} last_disconnect_detail={} last_detach_reason={} extension_user_agent={} bridge_popup_risk_suppression={} {} {} {} {} {} install_guidance={}",
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
            extension_build_sha256,
            EXPECTED_EXTENSION_BUILD_SHA256,
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

pub(crate) async fn click_node(
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

pub(crate) async fn type_node(
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

pub(crate) async fn node_value(
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

pub(crate) async fn open_tab(
    hwnd: i64,
    url: &str,
    agent_session_id: Option<&str>,
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

pub(crate) async fn list_tabs(
    hwnd: i64,
    expected_window_bounds: Option<Rect>,
    expected_window_title: Option<&str>,
) -> Result<ChromeDebuggerListTabsResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "listTabs")?;
    let result = bridge()
        .send_command(
            "listTabs",
            json!({
                "hwnd": hwnd,
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

pub(crate) async fn close_tab(
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

pub(crate) async fn target_info(
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

pub(crate) struct ChromeDebuggerViewportEmulationRequest<'a> {
    pub hwnd: i64,
    pub target_id: &'a str,
    pub operation: &'a str,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub device_scale_factor: Option<f64>,
    pub is_mobile: Option<bool>,
    pub wait_timeout_ms: u64,
}

pub(crate) async fn viewport_emulation(
    request: ChromeDebuggerViewportEmulationRequest<'_>,
) -> Result<ChromeDebuggerViewportEmulationResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(request.hwnd, "viewportEmulation")?;
    let result = bridge()
        .send_command(
            "viewportEmulation",
            json!({
                "hwnd": request.hwnd,
                "targetIdHint": request.target_id,
                "operation": request.operation,
                "width": request.width,
                "height": request.height,
                "deviceScaleFactor": request.device_scale_factor,
                "isMobile": request.is_mobile,
                "waitTimeoutMs": request.wait_timeout_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerViewportEmulationResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger viewportEmulation response: {error}"
        ))
    })
}

pub(crate) struct ChromeDebuggerDeviceEmulationRequest<'a> {
    pub hwnd: i64,
    pub target_id: &'a str,
    pub operation: &'a str,
    pub user_agent: Option<&'a str>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub device_scale_factor: Option<f64>,
    pub is_mobile: Option<bool>,
    pub has_touch: Option<bool>,
    pub max_touch_points: Option<u32>,
    pub wait_timeout_ms: u64,
}

pub(crate) async fn device_emulation(
    request: ChromeDebuggerDeviceEmulationRequest<'_>,
) -> Result<ChromeDebuggerDeviceEmulationResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(request.hwnd, "deviceEmulation")?;
    let result = bridge()
        .send_command(
            "deviceEmulation",
            json!({
                "hwnd": request.hwnd,
                "targetIdHint": request.target_id,
                "operation": request.operation,
                "userAgent": request.user_agent,
                "width": request.width,
                "height": request.height,
                "deviceScaleFactor": request.device_scale_factor,
                "isMobile": request.is_mobile,
                "hasTouch": request.has_touch,
                "maxTouchPoints": request.max_touch_points,
                "waitTimeoutMs": request.wait_timeout_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerDeviceEmulationResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger deviceEmulation response: {error}"
        ))
    })
}

pub(crate) struct ChromeDebuggerGeolocationEmulationRequest {
    pub hwnd: i64,
    pub target_id: String,
    pub operation: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub accuracy: Option<f64>,
    pub altitude: Option<f64>,
    pub altitude_accuracy: Option<f64>,
    pub heading: Option<f64>,
    pub speed: Option<f64>,
    pub grant_permission: Option<bool>,
    pub wait_timeout_ms: u64,
}

pub(crate) async fn geolocation_emulation(
    request: ChromeDebuggerGeolocationEmulationRequest,
) -> Result<ChromeDebuggerGeolocationEmulationResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(request.hwnd, "geolocationEmulation")?;
    let result = bridge()
        .send_command(
            "geolocationEmulation",
            json!({
                "hwnd": request.hwnd,
                "targetIdHint": request.target_id,
                "operation": request.operation,
                "latitude": request.latitude,
                "longitude": request.longitude,
                "accuracy": request.accuracy,
                "altitude": request.altitude,
                "altitudeAccuracy": request.altitude_accuracy,
                "heading": request.heading,
                "speed": request.speed,
                "grantPermission": request.grant_permission,
                "waitTimeoutMs": request.wait_timeout_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerGeolocationEmulationResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger geolocationEmulation response: {error}"
        ))
    })
}

pub(crate) struct ChromeDebuggerLocaleEmulationRequest<'a> {
    pub hwnd: i64,
    pub target_id: &'a str,
    pub operation: &'a str,
    pub locale: Option<&'a str>,
    pub timezone_id: Option<&'a str>,
    pub wait_timeout_ms: u64,
}

pub(crate) async fn locale_emulation(
    request: ChromeDebuggerLocaleEmulationRequest<'_>,
) -> Result<ChromeDebuggerLocaleEmulationResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(request.hwnd, "localeEmulation")?;
    let result = bridge()
        .send_command(
            "localeEmulation",
            json!({
                "hwnd": request.hwnd,
                "targetIdHint": request.target_id,
                "operation": request.operation,
                "locale": request.locale,
                "timezoneId": request.timezone_id,
                "waitTimeoutMs": request.wait_timeout_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerLocaleEmulationResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger localeEmulation response: {error}"
        ))
    })
}

pub(crate) struct ChromeDebuggerMediaEmulationRequest<'a> {
    pub hwnd: i64,
    pub target_id: &'a str,
    pub operation: &'a str,
    pub media: Option<&'a str>,
    pub color_scheme: Option<&'a str>,
    pub reduced_motion: Option<&'a str>,
    pub wait_timeout_ms: u64,
}

pub(crate) async fn media_emulation(
    request: ChromeDebuggerMediaEmulationRequest<'_>,
) -> Result<ChromeDebuggerMediaEmulationResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(request.hwnd, "mediaEmulation")?;
    let result = bridge()
        .send_command(
            "mediaEmulation",
            json!({
                "hwnd": request.hwnd,
                "targetIdHint": request.target_id,
                "operation": request.operation,
                "media": request.media,
                "colorScheme": request.color_scheme,
                "reducedMotion": request.reduced_motion,
                "waitTimeoutMs": request.wait_timeout_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerMediaEmulationResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger mediaEmulation response: {error}"
        ))
    })
}

pub(crate) struct ChromeDebuggerNetworkConditionsRequest<'a> {
    pub hwnd: i64,
    pub target_id: &'a str,
    pub operation: &'a str,
    pub offline: Option<bool>,
    pub latency_ms: Option<f64>,
    pub download_throughput_bytes_per_sec: Option<f64>,
    pub upload_throughput_bytes_per_sec: Option<f64>,
    pub connection_type: Option<&'a str>,
    pub wait_timeout_ms: u64,
}

pub(crate) async fn network_conditions(
    request: ChromeDebuggerNetworkConditionsRequest<'_>,
) -> Result<ChromeDebuggerNetworkConditionsResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(request.hwnd, "networkConditions")?;
    let result = bridge()
        .send_command(
            "networkConditions",
            json!({
                "hwnd": request.hwnd,
                "targetIdHint": request.target_id,
                "operation": request.operation,
                "offline": request.offline,
                "latencyMs": request.latency_ms,
                "downloadThroughputBytesPerSec": request.download_throughput_bytes_per_sec,
                "uploadThroughputBytesPerSec": request.upload_throughput_bytes_per_sec,
                "connectionType": request.connection_type,
                "waitTimeoutMs": request.wait_timeout_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerNetworkConditionsResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger networkConditions response: {error}"
        ))
    })
}

pub(crate) async fn frames(
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

pub(crate) async fn capture_visible_tab(
    hwnd: i64,
    _target_id: &str,
    _expected_chrome_window_id: Option<i64>,
) -> Result<ChromeDebuggerCaptureVisibleTabResult, ChromeDebuggerBridgeError> {
    Err(ChromeDebuggerBridgeError::normal_bridge_attach_disabled(
        hwnd,
        "capturePageScreenshot",
    ))
}

pub(crate) async fn page_screenshot(
    hwnd: i64,
    target_id: &str,
    params: Value,
) -> Result<ChromeDebuggerPageScreenshotResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "pageScreenshot")?;
    let mut payload = if params.is_object() {
        params
    } else {
        json!({})
    };
    payload["hwnd"] = json!(hwnd);
    payload["targetIdHint"] = json!(target_id);
    let result = bridge().send_command("pageScreenshot", payload).await?;
    serde_json::from_value::<ChromeDebuggerPageScreenshotResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger pageScreenshot response: {error}"
        ))
    })
}

pub(crate) async fn page_pdf(
    hwnd: i64,
    target_id: &str,
    params: Value,
) -> Result<ChromeDebuggerPagePdfResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "pagePdf")?;
    let mut payload = if params.is_object() {
        params
    } else {
        json!({})
    };
    payload["hwnd"] = json!(hwnd);
    payload["targetIdHint"] = json!(target_id);
    let result = bridge().send_command("pagePdf", payload).await?;
    serde_json::from_value::<ChromeDebuggerPagePdfResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger pagePdf response: {error}"
        ))
    })
}

pub(crate) async fn downloads(
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
    let result = bridge().send_command("downloads", payload).await?;
    serde_json::from_value::<ChromeDebuggerDownloadsResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger downloads response: {error}"
        ))
    })
}

pub(crate) async fn type_active_element(
    hwnd: i64,
    target_id: &str,
    text: &str,
) -> Result<ChromeDebuggerTypeActiveElementResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "typeActiveElement")?;
    let result = bridge()
        .send_command(
            "typeActiveElement",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
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
/// editable+visible match) or the current `active_element`, replaces its value
/// with the native prototype setter (React-safe), and returns the raw before/
/// after values for the daemon's Source-of-Truth check. No foreground, no
/// debugger attach; works on inactive/occluded tabs UIA cannot perceive.
pub(crate) async fn set_field_value(
    hwnd: i64,
    target_id: &str,
    selector: Option<&str>,
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
                "selector": selector,
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

pub(crate) async fn page_content(
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

pub(crate) async fn cookies(
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

pub(crate) async fn storage_state(
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

pub(crate) async fn set_content(
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

pub(crate) async fn aria_snapshot(
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
    serde_json::from_value::<ChromeDebuggerAriaSnapshotResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger ariaSnapshot response: {error}"
        ))
    })
}

pub(crate) async fn assert_poll(
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

pub(crate) async fn locate_elements(
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

pub(crate) async fn inspect_element(
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

pub(crate) async fn scroll_into_view(
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

pub(crate) async fn wait_for_text(
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

pub(crate) async fn wait_for_function(
    hwnd: i64,
    target_id: &str,
    expression: &str,
    args: Vec<Value>,
    timeout_ms: u64,
    polling_interval_ms: u64,
) -> Result<ChromeDebuggerWaitForFunctionResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "waitForFunction")?;
    let result = bridge()
        .send_command(
            "waitForFunction",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "expression": expression,
                "args": args,
                "timeoutMs": timeout_ms,
                "pollingIntervalMs": polling_interval_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerWaitForFunctionResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger waitForFunction response: {error}"
        ))
    })
}

pub(crate) async fn wait_for_load_state(
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

pub(crate) async fn wait_for_url(
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
pub(crate) async fn wait_for_request(
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
pub(crate) async fn wait_for_response(
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

pub(crate) async fn wait_for_selector(
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

pub(crate) async fn clock(
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

pub(crate) async fn page_events(
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

pub(crate) async fn navigate_tab(
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

/// Background-safe tab activation (#1189): selects `target_id` as the active
/// tab in its own Chrome window via `chrome.tabs.update({active:true})` without
/// taking the OS foreground. External debugger/nativeMessaging risks must be
/// suppressed by policy or the bridge management fallback before this queues.
pub(crate) async fn activate_tab(
    hwnd: i64,
    target_id: &str,
    wait_timeout_ms: u64,
) -> Result<ChromeDebuggerActivateTabResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "activateTab")?;
    let result = bridge()
        .send_command(
            "activateTab",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "waitTimeoutMs": wait_timeout_ms,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerActivateTabResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger activateTab response: {error}"
        ))
    })
}

pub(crate) async fn evaluate_script(
    hwnd: i64,
    target_id: &str,
    expression: &str,
    args: &[Value],
    await_promise: bool,
    return_by_value: bool,
) -> Result<ChromeDebuggerEvaluateScriptResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "evaluateScript")?;
    let result = bridge()
        .send_command(
            "evaluateScript",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "expression": expression,
                "args": args,
                "awaitPromise": await_promise,
                "returnByValue": return_by_value,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerEvaluateScriptResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger evaluateScript response: {error}"
        ))
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the MCP init-script parameters sent to the bridge"
)]
pub(crate) async fn init_script(
    hwnd: i64,
    target_id: &str,
    operation: &str,
    source: Option<&str>,
    identifier: Option<&str>,
    world_name: Option<&str>,
    include_command_line_api: bool,
    run_immediately: bool,
) -> Result<ChromeDebuggerInitScriptResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "initScript")?;
    let result = bridge()
        .send_command(
            "initScript",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "operation": operation,
                "source": source,
                "identifier": identifier,
                "worldName": world_name,
                "includeCommandLineAPI": include_command_line_api,
                "runImmediately": run_immediately,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerInitScriptResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger initScript response: {error}"
        ))
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the MCP binding parameters sent to the bridge"
)]
pub(crate) async fn expose_binding(
    hwnd: i64,
    target_id: &str,
    operation: &str,
    name: &str,
    execution_context_name: Option<&str>,
    since_seq: Option<u64>,
    max_calls: usize,
) -> Result<ChromeDebuggerExposeBindingResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "exposeBinding")?;
    let result = bridge()
        .send_command(
            "exposeBinding",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "operation": operation,
                "name": name,
                "executionContextName": execution_context_name,
                "sinceSeq": since_seq,
                "maxCalls": max_calls,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerExposeBindingResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger exposeBinding response: {error}"
        ))
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the MCP dialog parameters sent to the bridge"
)]
pub(crate) async fn handle_dialog(
    hwnd: i64,
    target_id: &str,
    operation: &str,
    default_policy: Option<&str>,
    prompt_text: Option<&str>,
    since_seq: Option<u64>,
    limit: usize,
) -> Result<ChromeDebuggerHandleDialogResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(hwnd, "handleDialog")?;
    let result = bridge()
        .send_command(
            "handleDialog",
            json!({
                "hwnd": hwnd,
                "targetIdHint": target_id,
                "operation": operation,
                "defaultPolicy": default_policy,
                "promptText": prompt_text,
                "sinceSeq": since_seq,
                "limit": limit,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerHandleDialogResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger handleDialog response: {error}"
        ))
    })
}

pub(crate) struct ChromeDebuggerFileUploadRequest<'a> {
    pub hwnd: i64,
    pub target_id: &'a str,
    pub operation: &'a str,
    pub files: &'a [String],
    pub selector: Option<&'a str>,
    pub element_id: Option<&'a str>,
    pub active_element: bool,
    pub since_seq: Option<u64>,
    pub limit: usize,
}

pub(crate) async fn file_upload(
    request: ChromeDebuggerFileUploadRequest<'_>,
) -> Result<ChromeDebuggerFileUploadResult, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(request.hwnd, "fileUpload")?;
    let result = bridge()
        .send_command(
            "fileUpload",
            json!({
                "hwnd": request.hwnd,
                "targetIdHint": request.target_id,
                "operation": request.operation,
                "files": request.files,
                "selector": request.selector,
                "elementId": request.element_id,
                "activeElement": request.active_element,
                "sinceSeq": request.since_seq,
                "limit": request.limit,
            }),
        )
        .await?;
    serde_json::from_value::<ChromeDebuggerFileUploadResult>(result).map_err(|error| {
        ChromeDebuggerBridgeError::protocol(format!(
            "decode Chrome debugger fileUpload response: {error}"
        ))
    })
}

pub(crate) struct ChromeDebuggerDomActionRequest<'a> {
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
}

pub(crate) async fn dom_action(
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
            }),
        )
        .await
}

pub(crate) struct ChromeDebuggerCdpInputRequest<'a> {
    pub hwnd: i64,
    pub target_id: &'a str,
    pub action: &'a str,
    pub selector: Option<&'a str>,
    pub element_id: Option<&'a str>,
    pub role: Option<&'a str>,
    pub name: Option<&'a str>,
    pub value: Option<&'a str>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub coordinate_space: Option<&'a str>,
    pub source_selector: Option<&'a str>,
    pub target_selector: Option<&'a str>,
    pub drag_steps: Option<u32>,
    pub drag_duration_ms: Option<u64>,
    pub drag_data_mime_type: Option<&'a str>,
    pub drag_data_text: Option<&'a str>,
    pub wait_timeout_ms: u64,
    pub auto_wait: bool,
    pub auto_wait_timeout_ms: u32,
}

pub(crate) async fn cdp_input(
    request: ChromeDebuggerCdpInputRequest<'_>,
) -> Result<Value, ChromeDebuggerBridgeError> {
    ensure_normal_bridge_external_popup_suppressed(request.hwnd, "cdpInput")?;
    bridge()
        .send_command(
            "cdpInput",
            json!({
                "hwnd": request.hwnd,
                "targetIdHint": request.target_id,
                "action": request.action,
                "selector": request.selector,
                "elementId": request.element_id,
                "role": request.role,
                "name": request.name,
                "value": request.value,
                "x": request.x,
                "y": request.y,
                "coordinateSpace": request.coordinate_space,
                "sourceSelector": request.source_selector,
                "targetSelector": request.target_selector,
                "dragSteps": request.drag_steps,
                "dragDurationMs": request.drag_duration_ms,
                "dragDataMimeType": request.drag_data_mime_type,
                "dragDataText": request.drag_data_text,
                "waitTimeoutMs": request.wait_timeout_ms,
                "autoWait": request.auto_wait,
                "autoWaitTimeoutMs": request.auto_wait_timeout_ms,
            }),
        )
        .await
}

pub(crate) struct ChromeDebuggerCoordinateClickRequest<'a> {
    pub hwnd: i64,
    pub target_id: &'a str,
    pub x: i32,
    pub y: i32,
    pub coordinate_space: &'a str,
    pub clicks: u8,
    pub button: Option<&'a str>,
    pub modifiers: Option<&'a Value>,
    pub wait_timeout_ms: u64,
}

pub(crate) async fn coordinate_click(
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
            }),
        )
        .await
}

pub(crate) fn validate_reload_wait_timeout(
    value: Option<u64>,
) -> Result<u64, ChromeDebuggerBridgeError> {
    let value = value.unwrap_or(DEFAULT_RELOAD_WAIT_TIMEOUT_MS);
    if value == 0 || value > MAX_RELOAD_WAIT_TIMEOUT_MS {
        return Err(ChromeDebuggerBridgeError::protocol(format!(
            "cdp_bridge_reload wait_timeout_ms must be 1..={MAX_RELOAD_WAIT_TIMEOUT_MS}"
        )));
    }
    Ok(value)
}

pub(crate) async fn reload_bridge(
    wait_timeout_ms: u64,
) -> Result<ChromeBridgeReloadResult, ChromeDebuggerBridgeError> {
    bridge().reload_self(wait_timeout_ms).await
}

pub(crate) fn is_direct_http_extension_bridge_request(headers: &HeaderMap, uri: &Uri) -> bool {
    let path = uri.path();
    if !path.starts_with("/chrome-debugger/native/") {
        return false;
    }
    if headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .is_some_and(|origin| origin == EXTENSION_ORIGIN)
    {
        return true;
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
    headers
        .get(BRIDGE_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|token| bridge().direct_http_bridge_token_matches(token))
}

pub(crate) async fn http_register(Json(request): Json<NativeRegisterRequest>) -> Response {
    if request.transport.as_deref() == Some("direct_http") {
        note_normal_bridge_registration_external_popup_risk();
    }
    match bridge().register(request) {
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
    }
}

pub(crate) async fn http_message(Json(request): Json<NativeMessageRequest>) -> Response {
    match bridge().post_message(request) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(detail) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "code": error_codes::A11Y_CDP_ATTACH_FAILED,
                "detail": detail,
            })),
        )
            .into_response(),
    }
}

pub(crate) async fn http_next(Query(query): Query<NativeNextQuery>) -> Response {
    let timeout_ms = query
        .timeout_ms
        .unwrap_or_else(|| u64::try_from(NATIVE_POLL_TIMEOUT.as_millis()).unwrap_or(15_000))
        .min(30_000);
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
    }
}

pub(crate) async fn http_ws(Query(query): Query<NativeWsQuery>, ws: WebSocketUpgrade) -> Response {
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
    ws.on_upgrade(move |socket| direct_http_ws_loop(socket, host_id))
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
                    Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) | Some(Ok(Message::Pong(_))) => {
                        if !bridge().touch_host(&host_id) {
                            disconnect_detail = "registered direct HTTP host disappeared while processing WebSocket keepalive".to_owned();
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

pub(crate) async fn run_native_host(
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

fn load_token_value() -> anyhow::Result<String> {
    match token_file_path() {
        Some(path) if path.is_file() => {
            let token = std::fs::read_to_string(&path)
                .with_context(|| format!("read HTTP bearer token file {}", path.display()))?;
            normalize_token(&token)
                .with_context(|| format!("HTTP bearer token file is empty: {}", path.display()))
        }
        Some(_) | None => {
            let token = std::env::var(TOKEN_ENV)
                .with_context(|| format!("{TOKEN_ENV} is unset and token.txt is absent"))?;
            normalize_token(&token).with_context(|| format!("{TOKEN_ENV} is empty"))
        }
    }
}

fn token_file_path() -> Option<PathBuf> {
    let appdata = std::env::var_os(APPDATA_ENV)?;
    Some(PathBuf::from(appdata).join("synapse").join("token.txt"))
}

fn normalize_token(raw: &str) -> anyhow::Result<String> {
    let token = raw.trim();
    if token.is_empty() {
        bail!("empty token")
    }
    Ok(token.to_owned())
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

fn digest_bridge_token(token: &str) -> [u8; 32] {
    let digest = Sha256::digest(token.as_bytes());
    let mut output = [0_u8; 32];
    output.copy_from_slice(&digest);
    output
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
            "url": result.get("url"),
            "target_attached": result.get("target_attached"),
            "target_count_before": result.get("target_count_before"),
            "target_count_after": result.get("target_count_after"),
            "extension_id": result.get("extension_id"),
        }),
        "closeTab" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "target_count_before": result.get("target_count_before"),
            "target_count_after": result.get("target_count_after"),
            "extension_id": result.get("extension_id"),
        }),
        "navigateTab" => json!({
            "target_id": result.get("target_id"),
            "tab_id": result.get("tab_id"),
            "action": result.get("action"),
            "requested_url": result.get("requested_url"),
            "before_url": result.get("before_url"),
            "after_url": result.get("after_url"),
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
            "url": result.get("url"),
            "title": result.get("title"),
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
            "url": result.get("url"),
            "title": result.get("title"),
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
            "url": result.get("url"),
            "title": result.get("title"),
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
            "before_url": result.get("before_url"),
            "after_url": result.get("after_url"),
            "after_title": result.get("after_title"),
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
            "url": result.get("url"),
            "title": result.get("title"),
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
    let queued_before = inner.commands.len();
    inner
        .commands
        .retain(|queued| queued.host_id != old_host_id);
    let queued_removed = queued_before.saturating_sub(inner.commands.len());
    let pending_ids = inner
        .pending
        .iter()
        .filter_map(|(id, pending)| {
            if pending.host_id == old_host_id {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for id in &pending_ids {
        if let Some(pending) = inner.pending.remove(id) {
            let _ = pending.sender.send(ChromeResponse {
                id: id.clone(),
                ok: false,
                result: None,
                error: Some(ChromeResponseError {
                    code: Some(error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE.to_owned()),
                    detail: Some(format!(
                        "Chrome debugger direct HTTP bridge host {old_host_id} was replaced by {new_host_id} before command response"
                    )),
                }),
            });
        }
    }
    tracing::warn!(
        code = "CHROME_DEBUGGER_DIRECT_HTTP_HOST_REPLACED",
        old_host_id = %old_host_id,
        new_host_id = %new_host_id,
        queued_removed,
        pending_failed = pending_ids.len(),
        "Chrome debugger direct HTTP bridge host replaced"
    );
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, Uri, header};

    use super::*;

    #[test]
    fn native_host_invocation_detects_chrome_origin_and_parent_window() {
        let invocation = native_host_invocation_from_args([
            OsString::from("chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/"),
            OsString::from("--parent-window=1234"),
        ])
        .expect("chrome native host origin should be detected");

        assert_eq!(
            invocation.origin,
            "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/"
        );
        assert_eq!(invocation.parent_window.as_deref(), Some("1234"));
    }

    #[test]
    fn native_host_unknown_id_error_is_restart_recoverable() {
        let detail = r#"{"ok":false,"code":"A11Y_CDP_EXTENSION_UNAVAILABLE","detail":"unknown chrome debugger native host_id \"chrome-native-old\""}"#;
        let error =
            anyhow::anyhow!("Chrome debugger native poll failed status=400 detail={detail}");

        assert!(is_unknown_native_host_detail(detail));
        assert!(is_unknown_native_host_error(&error));
        assert!(!is_unknown_native_host_detail("bridge protocol mismatch"));
    }

    #[test]
    fn extension_error_preserves_dom_action_codes() {
        for code in [
            error_codes::CHROME_SCRIPTING_EXECUTE_FAILED,
            error_codes::CHROME_DOM_SELECTOR_INVALID,
            error_codes::CHROME_DOM_ELEMENT_NOT_FOUND,
            error_codes::CHROME_DOM_ELEMENT_AMBIGUOUS,
            error_codes::CHROME_DOM_ELEMENT_NOT_ACTIONABLE,
            error_codes::CHROME_DOM_ACTION_UNSUPPORTED,
            error_codes::CHROME_DOM_ACTION_POSTCONDITION_FAILED,
            error_codes::ACTION_TARGET_INVALID,
        ] {
            let error = ChromeDebuggerBridgeError::extension(Some(code), "dom action failed");
            assert_eq!(error.code(), code);
            assert_eq!(error.detail(), "dom action failed");
        }
    }

    #[test]
    fn direct_http_bridge_token_authorizes_next_without_origin_only_after_register() {
        let registered = bridge()
            .register(NativeRegisterRequest {
                origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
                pid: 0,
                parent_window: None,
                bridge_protocol_version: BRIDGE_PROTOCOL_VERSION,
                transport: Some("direct_http".to_owned()),
            })
            .expect("direct bridge register should issue a host token");
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:7700"));
        headers.insert(
            BRIDGE_TOKEN_HEADER,
            HeaderValue::from_str(&registered.bridge_token)
                .expect("bridge token should be a valid header value"),
        );

        assert!(is_direct_http_extension_bridge_request(
            &headers,
            &Uri::from_static("/chrome-debugger/native/next?host_id=anything"),
        ));
        let ws_uri = format!(
            "/chrome-debugger/native/ws?host_id={}&bridge_token={}",
            registered.host_id, registered.bridge_token
        )
        .parse::<Uri>()
        .expect("websocket uri with token should parse");
        assert!(is_direct_http_extension_bridge_request(
            &HeaderMap::new(),
            &ws_uri
        ));
        assert!(!is_direct_http_extension_bridge_request(
            &headers,
            &Uri::from_static("/chrome-debugger/native/register"),
        ));
        assert!(!is_direct_http_extension_bridge_request(
            &headers,
            &Uri::from_static("/mcp"),
        ));
    }

    #[test]
    fn extension_unavailable_maps_to_explicit_cdp_status() {
        let error = ChromeDebuggerBridgeError::unavailable();

        assert_eq!(error.code(), error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE);
        assert_eq!(error.cdp_status(), CdpStatus::ExtensionUnavailable);
        assert!(
            error
                .detail()
                .contains("install the bundled Synapse Chrome extension")
        );
        assert!(error.detail().contains("no_active_host_repair="));
        assert!(
            error
                .detail()
                .contains("do not launch a second Chrome process/profile")
        );
    }

    #[test]
    fn chrome_bridge_health_reports_unavailable_without_active_host() {
        let health = chrome_bridge_health_from_snapshot(None, 0, 0, 0, &[], &[], &[]);

        assert_eq!(health.status, "unavailable");
        let detail = health.detail.as_deref().expect("health detail");
        assert!(detail.contains("tab_control_available=false"));
        assert!(detail.contains("reason=no_active_chrome_bridge_host"));
        assert!(detail.contains("expected_extension_id=leoocgnkjnplbfdbklajepahofecgfbk"));
        assert!(detail.contains("repair_guidance=no_active_host_repair="));
        assert!(detail.contains("already-open authenticated Chrome profile"));
        assert!(detail.contains("do not launch a second Chrome process/profile"));
        assert!(detail.contains("cdp_bridge_reload"));
        assert!(detail.contains("scripts\\install-synapse-chrome-debugger.ps1"));
    }

    #[test]
    fn chrome_bridge_health_reports_absent_profile_install_without_active_host() {
        let self_policy_shield = SynapseChromeSelfPolicyShieldStatus {
            present: true,
            detail: "synapse_chrome_self_policy_shield_present=true reason=test_present".to_owned(),
        };
        let profile_install_state = SynapseChromeProfileInstallState {
            detail: "synapse_chrome_bridge_profile_installation scanned=true installed=false profile_count=6 installed_profile_count=0 active_profile=\"Profile 5\" active_profile_installed=false reason=extension_id_absent_from_preferences_and_secure_preferences cdp_bridge_reload_can_install_absent_extension=false remediation=test".to_owned(),
        };
        let health = chrome_bridge_health_from_snapshot_with_self_policy(
            None,
            0,
            0,
            0,
            &[],
            &[],
            &[],
            &self_policy_shield,
            &profile_install_state,
        );

        assert_eq!(health.status, "unavailable");
        let detail = health.detail.as_deref().expect("health detail");
        assert!(detail.contains("reason=no_active_chrome_bridge_host"));
        assert!(detail.contains("synapse_chrome_bridge_profile_installation"));
        assert!(detail.contains("installed=false"));
        assert!(detail.contains("installed_profile_count=0"));
        assert!(detail.contains("active_profile_installed=false"));
        assert!(detail.contains("cdp_bridge_reload_can_install_absent_extension=false"));
    }

    #[test]
    fn chrome_bridge_health_reports_external_popup_risk_unknown_without_active_host() {
        let health = chrome_bridge_health_from_snapshot(
            None,
            0,
            0,
            0,
            &["profile=Default extension_id=external active_api=debugger".to_owned()],
            &[],
            &[],
        );

        assert_eq!(health.status, "unavailable");
        let detail = health.detail.as_deref().expect("health detail");
        assert!(detail.contains("reason=no_active_chrome_bridge_host"));
        assert!(detail.contains("external_chrome_popup_risk_warning=true"));
        assert!(
            detail.contains("external_chrome_popup_risk_scope=host_unavailable_no_live_management")
        );
        assert!(detail.contains("extension_id=external"));
        assert!(!detail.contains("external_chrome_popup_risk_blocking=true"));
        assert!(!detail.contains("external_chrome_popup_risk_scope=external_suppression_required"));
    }

    #[test]
    fn chrome_bridge_health_reports_ready_active_host() {
        let host = ChromeBridgeHealthRecord {
            host_id: "chrome-native-test".to_owned(),
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
            extension_version: Some("0.1.0".to_owned()),
            extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
            extension_build_id: Some(EXPECTED_EXTENSION_BUILD_ID.to_owned()),
            extension_build_sha256: Some(EXPECTED_EXTENSION_BUILD_SHA256.to_owned()),
            extension_capabilities: REQUIRED_DIRECT_HTTP_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            extension_user_agent: Some("Chrome test".to_owned()),
            extension_debugger_api_available: Some(true),
            extension_popup_risk_suppression: Some(json!({
                "ok": true,
                "status": "clear",
                "management_available": true,
                "hazard_count": 0,
                "disabled_count": 0,
                "remaining_hazard_count": 0,
                "failure_count": 0,
                "remaining_hazards": [],
                "failures": []
            })),
            pid: 42,
            parent_window: Some("1001".to_owned()),
            transport: Some("direct_http".to_owned()),
            registered_unix_ms: 1000,
            last_seen_unix_ms: 2000,
            last_disconnect_detail: None,
            last_detach_reason: None,
        };

        let health = chrome_bridge_health_from_snapshot(Some(&host), 1, 2, 3, &[], &[], &[]);

        assert_eq!(health.status, "ok");
        let detail = health.detail.as_deref().expect("health detail");
        assert!(detail.contains("tab_control_available=true"));
        assert!(detail.contains("active_host_id=chrome-native-test"));
        assert!(
            detail.contains(
                "endpoint=chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/chrome.tabs"
            )
        );
        assert!(detail.contains("queued_count=2"));
        assert!(detail.contains("pending_count=3"));
        assert!(detail.contains("extension_debugger_api_available=true"));
    }

    #[test]
    fn chrome_bridge_health_blocks_runtime_debugger_api_unavailable() {
        let host = ChromeBridgeHealthRecord {
            host_id: "chrome-native-test".to_owned(),
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
            extension_version: Some("0.1.0".to_owned()),
            extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
            extension_build_id: Some(EXPECTED_EXTENSION_BUILD_ID.to_owned()),
            extension_build_sha256: Some(EXPECTED_EXTENSION_BUILD_SHA256.to_owned()),
            extension_capabilities: REQUIRED_DIRECT_HTTP_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            extension_user_agent: Some("Chrome test".to_owned()),
            extension_debugger_api_available: Some(false),
            extension_popup_risk_suppression: Some(json!({
                "ok": true,
                "status": "clear",
                "management_available": true,
                "hazard_count": 0,
                "disabled_count": 0,
                "remaining_hazard_count": 0,
                "failure_count": 0,
                "remaining_hazards": [],
                "failures": []
            })),
            pid: 42,
            parent_window: Some("1001".to_owned()),
            transport: Some("direct_http".to_owned()),
            registered_unix_ms: 1000,
            last_seen_unix_ms: 2000,
            last_disconnect_detail: None,
            last_detach_reason: None,
        };

        let health = chrome_bridge_health_from_snapshot(Some(&host), 1, 0, 0, &[], &[], &[]);

        assert_eq!(health.status, "stale");
        let detail = health.detail.as_deref().expect("health detail");
        assert!(detail.contains("tab_control_available=false"));
        assert!(detail.contains("extension_debugger_api_available=false"));
        assert!(detail.contains("debugger_api_available=false expected=true"));
    }

    #[test]
    fn chrome_bridge_health_blocks_external_popup_risk_until_suppressed() {
        let host = ChromeBridgeHealthRecord {
            host_id: "chrome-native-test".to_owned(),
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
            extension_version: Some("0.1.0".to_owned()),
            extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
            extension_build_id: Some(EXPECTED_EXTENSION_BUILD_ID.to_owned()),
            extension_build_sha256: Some(EXPECTED_EXTENSION_BUILD_SHA256.to_owned()),
            extension_capabilities: REQUIRED_DIRECT_HTTP_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            extension_user_agent: Some("Chrome test".to_owned()),
            extension_debugger_api_available: Some(true),
            extension_popup_risk_suppression: None,
            pid: 42,
            parent_window: None,
            transport: Some("direct_http".to_owned()),
            registered_unix_ms: 1000,
            last_seen_unix_ms: 2000,
            last_disconnect_detail: None,
            last_detach_reason: None,
        };

        let health = chrome_bridge_health_from_snapshot(
            Some(&host),
            1,
            0,
            0,
            &["profile=Default extension_id=external active_api=debugger".to_owned()],
            &[],
            &[],
        );

        assert_eq!(health.status, "unsafe_profile");
        let detail = health.detail.as_deref().expect("health detail");
        assert!(detail.contains("tab_control_available=false"));
        assert!(detail.contains("external_chrome_popup_risk_blocking=true"));
        assert!(detail.contains("external_chrome_popup_risk_scope=external_suppression_required"));
        assert!(detail.contains("extension_id=external"));
    }

    #[test]
    fn chrome_bridge_health_allows_external_popup_risk_when_bridge_management_suppressed() {
        let host = ChromeBridgeHealthRecord {
            host_id: "chrome-native-test".to_owned(),
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
            extension_version: Some("0.1.0".to_owned()),
            extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
            extension_build_id: Some(EXPECTED_EXTENSION_BUILD_ID.to_owned()),
            extension_build_sha256: Some(EXPECTED_EXTENSION_BUILD_SHA256.to_owned()),
            extension_capabilities: REQUIRED_DIRECT_HTTP_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            extension_user_agent: Some("Chrome test".to_owned()),
            extension_debugger_api_available: Some(true),
            extension_popup_risk_suppression: Some(json!({
                "ok": true,
                "status": "suppressed",
                "management_available": true,
                "hazard_count": 1,
                "disabled_count": 1,
                "remaining_hazard_count": 0,
                "failure_count": 0,
                "remaining_hazards": [],
                "failures": []
            })),
            pid: 42,
            parent_window: None,
            transport: Some("direct_http".to_owned()),
            registered_unix_ms: 1000,
            last_seen_unix_ms: 2000,
            last_disconnect_detail: None,
            last_detach_reason: None,
        };

        let health = chrome_bridge_health_from_snapshot(
            Some(&host),
            1,
            0,
            0,
            &["profile=Default extension_id=external active_api=debugger".to_owned()],
            &[],
            &[],
        );

        assert_eq!(health.status, "ok");
        let detail = health.detail.as_deref().expect("health detail");
        assert!(detail.contains("tab_control_available=true"));
        assert!(detail.contains("external_chrome_popup_risk_warning=true"));
        assert!(
            detail.contains("external_chrome_popup_risk_scope=covered_by_live_bridge_management")
        );
        assert!(detail.contains("bridge_popup_risk_suppression=status=suppressed"));
    }

    #[test]
    fn chrome_bridge_health_allows_physical_profile_risk_when_live_management_is_clear() {
        let host = ChromeBridgeHealthRecord {
            host_id: "chrome-native-test".to_owned(),
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
            extension_version: Some("0.1.0".to_owned()),
            extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
            extension_build_id: Some(EXPECTED_EXTENSION_BUILD_ID.to_owned()),
            extension_build_sha256: Some(EXPECTED_EXTENSION_BUILD_SHA256.to_owned()),
            extension_capabilities: REQUIRED_DIRECT_HTTP_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            extension_user_agent: Some("Chrome test".to_owned()),
            extension_debugger_api_available: Some(true),
            extension_popup_risk_suppression: Some(json!({
                "ok": true,
                "status": "clear",
                "management_available": true,
                "hazard_count": 0,
                "disabled_count": 0,
                "remaining_hazard_count": 0,
                "failure_count": 0,
                "remaining_hazards": [],
                "failures": []
            })),
            pid: 42,
            parent_window: None,
            transport: Some("direct_http".to_owned()),
            registered_unix_ms: 1000,
            last_seen_unix_ms: 2000,
            last_disconnect_detail: None,
            last_detach_reason: None,
        };

        let health = chrome_bridge_health_from_snapshot(
            Some(&host),
            1,
            0,
            0,
            &["profile=Profile 5 extension_id=fcoeoabgfenejglbffodgkkbkcdhcgfn active_api=debugger,nativeMessaging popup_risk=true".to_owned()],
            &[],
            &[],
        );

        assert_eq!(health.status, "ok");
        let detail = health.detail.as_deref().expect("health detail");
        println!(
            "readback=chrome_bridge_health edge=live_management_clear_over_physical_profile_risk detail={detail}"
        );
        assert!(detail.contains("tab_control_available=true"));
        assert!(detail.contains("external_chrome_popup_risk_warning=true"));
        assert!(
            detail.contains("external_chrome_popup_risk_scope=covered_by_live_bridge_management")
        );
        assert!(detail.contains("bridge_popup_risk_suppression=status=clear"));
        assert!(!detail.contains("external_chrome_popup_risk_blocking=true"));
    }

    #[test]
    fn chrome_bridge_health_reports_external_layout_infobar_risk_as_warning() {
        let host = ChromeBridgeHealthRecord {
            host_id: "chrome-native-test".to_owned(),
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
            extension_version: Some("0.1.0".to_owned()),
            extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
            extension_build_id: Some(EXPECTED_EXTENSION_BUILD_ID.to_owned()),
            extension_build_sha256: Some(EXPECTED_EXTENSION_BUILD_SHA256.to_owned()),
            extension_capabilities: REQUIRED_DIRECT_HTTP_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            extension_user_agent: Some("Chrome test".to_owned()),
            extension_debugger_api_available: Some(true),
            extension_popup_risk_suppression: Some(json!({
                "ok": true,
                "status": "clear",
                "management_available": true,
                "hazard_count": 0,
                "disabled_count": 0,
                "remaining_hazard_count": 0,
                "failure_count": 0,
                "remaining_hazards": [],
                "failures": []
            })),
            pid: 42,
            parent_window: None,
            transport: Some("direct_http".to_owned()),
            registered_unix_ms: 1000,
            last_seen_unix_ms: 2000,
            last_disconnect_detail: None,
            last_detach_reason: None,
        };

        let health = chrome_bridge_health_from_snapshot(
            Some(&host),
            1,
            0,
            0,
            &[],
            &[],
            &["chrome_process pid=66452 parent_pid=100 parent_chain=100:node.exe name=chrome.exe reasons=headed_ms_playwright_mcp_layout_banner,remote_debugging_without_silent_debugger_extension_api user_data_dir=\"C:\\Users\\hotra\\AppData\\Local\\ms-playwright-mcp\\profile\" user_data_dir_state=dedicated_or_external owner_hint=ms_playwright_mcp_external repair_hint=stop_external_playwright_mcp_chrome_or_relaunch_with_popup_safe_flags has_remote_debugging_pipe=false has_remote_debugging_port=true has_silent_debugger_extension_api=false has_ms_playwright_mcp_dir=true command_metadata_policy=safe_display_v1 command_line_len=256 command_line_sha256=sha256:abc123".to_owned()],
        );

        assert_eq!(health.status, "ok");
        let detail = health.detail.as_deref().expect("health detail");
        assert!(detail.contains("tab_control_available=true"));
        assert!(detail.contains("external_chrome_layout_infobar_risk_warning=true"));
        assert!(detail.contains("layout_risk_count=1"));
        assert!(detail.contains("headed_ms_playwright_mcp_layout_banner"));
        assert!(detail.contains("parent_chain=100:node.exe"));
        assert!(detail.contains("user_data_dir_state=dedicated_or_external"));
        assert!(detail.contains("owner_hint=ms_playwright_mcp_external"));
        assert!(detail.contains(
            "repair_hint=stop_external_playwright_mcp_chrome_or_relaunch_with_popup_safe_flags"
        ));
        assert!(detail.contains("command_metadata_policy=safe_display_v1"));
        assert!(detail.contains("command_line_sha256=sha256:abc123"));
    }

    #[test]
    fn chrome_layout_infobar_metadata_parses_and_classifies_owner() {
        let args = vec![
            "chrome.exe".to_owned(),
            "--remote-debugging-port=9222".to_owned(),
            "--user-data-dir=C:\\Temp\\ms-playwright-mcp\\profile".to_owned(),
        ];
        assert_eq!(
            process_switch_arg_value(&args, "--user-data-dir").as_deref(),
            Some("C:\\Temp\\ms-playwright-mcp\\profile")
        );
        assert_eq!(
            chrome_layout_infobar_owner_hint(
                "chrome.exe --remote-debugging-port=9222",
                Some("C:\\Temp\\ms-playwright-mcp\\profile"),
                "100:node.exe"
            ),
            "ms_playwright_mcp_external"
        );
        assert_eq!(
            chrome_layout_infobar_repair_hint("ms_playwright_mcp_external"),
            "stop_external_playwright_mcp_chrome_or_relaunch_with_popup_safe_flags"
        );
        assert_eq!(
            chrome_layout_infobar_owner_hint(
                "chrome.exe --remote-debugging-pipe",
                Some("C:\\Users\\hotra\\AppData\\Local\\synapse\\synapse-cdp-profiles\\agent"),
                "200:synapse-mcp.exe"
            ),
            "synapse_owned_or_spawned"
        );
        assert_eq!(
            chrome_layout_infobar_repair_hint("synapse_owned_or_spawned"),
            "terminate_exact_synapse_owned_pid_tree_or_session_cleanup"
        );
        assert_eq!(
            chrome_layout_infobar_owner_hint(
                "chrome.exe --remote-debugging-port=9222",
                None,
                "300:powershell.exe"
            ),
            "unknown_external"
        );
        assert_eq!(
            chrome_layout_infobar_repair_hint("unknown_external"),
            "do_not_attach_or_target_until_owner_identified"
        );
        assert_eq!(quote_detail_value("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn chrome_bridge_health_blocks_stale_active_self_permission() {
        let host = ChromeBridgeHealthRecord {
            host_id: "chrome-native-test".to_owned(),
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
            extension_version: Some("0.1.0".to_owned()),
            extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
            extension_build_id: Some(EXPECTED_EXTENSION_BUILD_ID.to_owned()),
            extension_build_sha256: Some(EXPECTED_EXTENSION_BUILD_SHA256.to_owned()),
            extension_capabilities: REQUIRED_DIRECT_HTTP_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            extension_user_agent: Some("Chrome test".to_owned()),
            extension_debugger_api_available: Some(true),
            extension_popup_risk_suppression: Some(json!({
                "ok": true,
                "status": "clear",
                "management_available": true,
                "hazard_count": 0,
                "disabled_count": 0,
                "remaining_hazard_count": 0,
                "failure_count": 0,
                "remaining_hazards": [],
                "failures": []
            })),
            pid: 42,
            parent_window: None,
            transport: Some("direct_http".to_owned()),
            registered_unix_ms: 1000,
            last_seen_unix_ms: 2000,
            last_disconnect_detail: None,
            last_detach_reason: None,
        };

        let self_rows = [format!(
            "profile=Default pref=Preferences extension_id={EXTENSION_ID} name=\"Synapse Chrome Bridge\" active_api=nativeMessaging manifest_api=nativeMessaging granted_hazard_api=nativeMessaging synapse_self_popup_risk=true risk_basis=active_or_manifest_hazard_without_disable_reason state=1 active_bit=true disable_reasons=[]"
        )];
        let health = chrome_bridge_health_from_snapshot(Some(&host), 1, 0, 0, &[], &self_rows, &[]);

        assert_eq!(health.status, "unsafe_profile");
        let detail = health.detail.as_deref().expect("health detail");
        assert!(detail.contains("tab_control_available=false"));
        assert!(detail.contains("synapse_chrome_bridge_permission_blocking=true"));
        assert!(detail.contains("active_synapse_bridge_native_messaging_permission"));
    }

    #[test]
    fn chrome_bridge_health_warns_on_self_granted_only_residue_without_policy_shield() {
        let host = ChromeBridgeHealthRecord {
            host_id: "chrome-native-test".to_owned(),
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
            extension_version: Some("0.1.0".to_owned()),
            extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
            extension_build_id: Some(EXPECTED_EXTENSION_BUILD_ID.to_owned()),
            extension_build_sha256: Some(EXPECTED_EXTENSION_BUILD_SHA256.to_owned()),
            extension_capabilities: REQUIRED_DIRECT_HTTP_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            extension_user_agent: Some("Chrome test".to_owned()),
            extension_debugger_api_available: Some(true),
            extension_popup_risk_suppression: Some(json!({
                "ok": true,
                "status": "clear",
                "management_available": true,
                "hazard_count": 0,
                "disabled_count": 0,
                "remaining_hazard_count": 0,
                "failure_count": 0,
                "remaining_hazards": [],
                "failures": []
            })),
            pid: 42,
            parent_window: None,
            transport: Some("direct_http".to_owned()),
            registered_unix_ms: 1000,
            last_seen_unix_ms: 2000,
            last_disconnect_detail: None,
            last_detach_reason: None,
        };

        let self_rows = [format!(
            "profile=Default pref=Secure Preferences extension_id={EXTENSION_ID} name=\"Synapse Chrome Bridge\" active_api=alarms,tabs,debugger manifest_api=alarms,tabs,debugger granted_hazard_api=nativeMessaging synapse_self_popup_risk=true risk_basis=granted_only_stale state=<absent> active_bit=<absent> disable_reasons=[]"
        )];
        let missing_policy_shield = SynapseChromeSelfPolicyShieldStatus {
            present: false,
            detail: "synapse_chrome_self_policy_shield_present=false reason=test_missing"
                .to_owned(),
        };
        let profile_install_state = SynapseChromeProfileInstallState::not_scanned("test");
        let health = chrome_bridge_health_from_snapshot_with_self_policy(
            Some(&host),
            1,
            0,
            0,
            &[],
            &self_rows,
            &[],
            &missing_policy_shield,
            &profile_install_state,
        );

        assert_eq!(health.status, "ok");
        let detail = health.detail.as_deref().expect("health detail");
        assert!(detail.contains("tab_control_available=true"));
        assert!(detail.contains("synapse_chrome_bridge_permission_warning=true"));
        assert!(detail.contains("granted_only_stale_permissions_without_policy_shield"));
        assert!(detail.contains("synapse_chrome_self_policy_shield_present=false"));
        assert!(detail.contains("extension_debugger_api_available=true"));
    }

    #[test]
    fn chrome_bridge_health_warns_on_self_granted_only_residue_with_policy_shield() {
        let host = ChromeBridgeHealthRecord {
            host_id: "chrome-native-test".to_owned(),
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
            extension_version: Some("0.1.0".to_owned()),
            extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
            extension_build_id: Some(EXPECTED_EXTENSION_BUILD_ID.to_owned()),
            extension_build_sha256: Some(EXPECTED_EXTENSION_BUILD_SHA256.to_owned()),
            extension_capabilities: REQUIRED_DIRECT_HTTP_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            extension_user_agent: Some("Chrome test".to_owned()),
            extension_debugger_api_available: Some(true),
            extension_popup_risk_suppression: Some(json!({
                "ok": true,
                "status": "clear",
                "management_available": true,
                "hazard_count": 0,
                "disabled_count": 0,
                "remaining_hazard_count": 0,
                "failure_count": 0,
                "remaining_hazards": [],
                "failures": []
            })),
            pid: 42,
            parent_window: None,
            transport: Some("direct_http".to_owned()),
            registered_unix_ms: 1000,
            last_seen_unix_ms: 2000,
            last_disconnect_detail: None,
            last_detach_reason: None,
        };

        let self_rows = [format!(
            "profile=Default pref=Secure Preferences extension_id={EXTENSION_ID} name=\"Synapse Chrome Bridge\" active_api=alarms,tabs,debugger manifest_api=alarms,tabs,debugger granted_hazard_api=nativeMessaging synapse_self_popup_risk=true risk_basis=granted_only_stale state=<absent> active_bit=<absent> disable_reasons=[]"
        )];
        let present_policy_shield = SynapseChromeSelfPolicyShieldStatus {
            present: true,
            detail: "synapse_chrome_self_policy_shield_present=true reason=test_present".to_owned(),
        };
        let profile_install_state = SynapseChromeProfileInstallState::not_scanned("test");
        let health = chrome_bridge_health_from_snapshot_with_self_policy(
            Some(&host),
            1,
            0,
            0,
            &[],
            &self_rows,
            &[],
            &present_policy_shield,
            &profile_install_state,
        );

        assert_eq!(health.status, "ok");
        let detail = health.detail.as_deref().expect("health detail");
        assert!(detail.contains("tab_control_available=true"));
        assert!(detail.contains("synapse_chrome_bridge_permission_warning=true"));
        assert!(detail.contains("granted_only_stale_permissions_with_policy_shield"));
        assert!(detail.contains("synapse_chrome_self_policy_shield_present=true"));
    }

    #[test]
    fn direct_http_bridge_refuses_new_commands_without_capability_readback() {
        let mut host = HostRecord {
            origin: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/".to_owned(),
            extension_id: Some(EXTENSION_ID.to_owned()),
            extension_version: None,
            extension_protocol_version: None,
            extension_build_id: None,
            extension_build_sha256: None,
            extension_capabilities: BTreeSet::new(),
            extension_user_agent: None,
            extension_debugger_api_available: None,
            extension_popup_risk_suppression: None,
            pid: 42,
            parent_window: None,
            transport: Some("direct_http".to_owned()),
            bridge_token_digest: [0; 32],
            registered_unix_ms: 1000,
            last_seen_unix_ms: 2000,
            last_disconnect_detail: None,
            last_detach_reason: None,
        };

        let reason = bridge_command_stale_reason(&host, "openTab").expect("missing identity");
        assert!(reason.contains("build_id=not_seen_yet"));
        assert!(reason.contains("build_sha256=not_seen_yet"));
        let reason = bridge_command_stale_reason(&host, "reloadSelf").expect("stale reason");
        assert!(reason.contains("missing_capability=reloadSelf"));

        host.extension_capabilities = ["openTab", "closeTab", "targetInfo"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let reason = bridge_command_stale_reason(&host, "targetInfoPageText")
            .expect("missing identity still fails before capability fallback");
        assert!(reason.contains("build_id=not_seen_yet"));
        let reason = bridge_command_stale_reason(&host, "reloadSelf").expect("missing capability");
        assert!(reason.contains("missing_capability=reloadSelf"));

        host.extension_build_id = Some("old-build".to_owned());
        host.extension_build_sha256 = Some("old-sha".to_owned());
        host.extension_capabilities = REQUIRED_DIRECT_HTTP_CAPABILITIES
            .iter()
            .map(|capability| (*capability).to_owned())
            .collect();
        let reason = bridge_command_stale_reason(&host, "openTab").expect("stale build blocked");
        assert!(reason.contains("build_id=old-build"));
        assert!(reason.contains("build_sha256=old-sha"));
        assert_eq!(bridge_command_stale_reason(&host, "reloadSelf"), None);

        host.extension_build_id = Some(EXPECTED_EXTENSION_BUILD_ID.to_owned());
        host.extension_build_sha256 = Some(EXPECTED_EXTENSION_BUILD_SHA256.to_owned());
        let reason = bridge_command_stale_reason(&host, "openTab")
            .expect("missing runtime debugger API readback is unsafe");
        assert!(reason.contains("debugger_api_available=not_seen_yet"));

        host.extension_debugger_api_available = Some(true);
        assert_eq!(bridge_command_stale_reason(&host, "openTab"), None);

        host.extension_debugger_api_available = Some(false);
        let reason = bridge_command_stale_reason(&host, "openTab")
            .expect("runtime debugger API availability false is unsafe");
        assert!(reason.contains("debugger_api_available=false"));

        host.extension_debugger_api_available = Some(true);
        host.extension_capabilities.clear();
        let reason = bridge_command_stale_reason(&host, "openTab")
            .expect("exact identity without capability readback is still unsafe");
        assert!(reason.contains("capabilities_not_advertised"));
        assert!(reason.contains("required=alarmReconnect"));

        host.extension_capabilities = ["openTab", "closeTab", "targetInfo"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let reason = bridge_command_stale_reason(&host, "targetInfoPageText")
            .expect("missing targetInfoPageText capability");
        assert!(reason.contains("missing_capability=targetInfoPageText"));

        host.extension_capabilities = REQUIRED_DIRECT_HTTP_CAPABILITIES
            .iter()
            .map(|capability| (*capability).to_owned())
            .collect();
        assert_eq!(bridge_command_stale_reason(&host, "reloadSelf"), None);
        assert_eq!(bridge_command_stale_reason(&host, "openTab"), None);
    }

    #[test]
    fn reload_self_uses_loaded_build_id_as_command_guard() {
        let mut snapshot = ChromeBridgeHostSnapshot {
            host_id: "chrome-native-stale".to_owned(),
            origin: format!("chrome-extension://{EXTENSION_ID}/"),
            extension_id: Some(EXTENSION_ID.to_owned()),
            extension_version: Some("0.1.0".to_owned()),
            extension_protocol_version: Some(BRIDGE_PROTOCOL_VERSION),
            extension_build_id: Some("synapse-chrome-bridge-older-but-reload-capable".to_owned()),
            extension_build_sha256: Some("old-sha".to_owned()),
            extension_capabilities: vec!["reloadSelf".to_owned()],
            extension_user_agent: Some("Chrome test".to_owned()),
            extension_debugger_api_available: Some(false),
            extension_popup_risk_suppression: None,
            pid: 0,
            parent_window: None,
            transport: Some("direct_http".to_owned()),
            registered_unix_ms: 1000,
            last_seen_unix_ms: 2000,
            last_disconnect_detail: None,
            last_detach_reason: None,
            extension_stale: true,
            extension_stale_reasons: vec!["build_id=old expected=new".to_owned()],
        };

        assert_eq!(
            reload_self_expected_loaded_build_id(&snapshot),
            Some("synapse-chrome-bridge-older-but-reload-capable")
        );

        snapshot.extension_build_id = Some(String::new());
        assert_eq!(reload_self_expected_loaded_build_id(&snapshot), None);

        snapshot.extension_build_id = None;
        assert_eq!(reload_self_expected_loaded_build_id(&snapshot), None);
    }

    #[test]
    fn reload_wait_timeout_validation_is_bounded() {
        assert_eq!(
            validate_reload_wait_timeout(None).expect("default accepted"),
            DEFAULT_RELOAD_WAIT_TIMEOUT_MS
        );
        assert_eq!(validate_reload_wait_timeout(Some(1)).expect("min"), 1);
        assert_eq!(
            validate_reload_wait_timeout(Some(MAX_RELOAD_WAIT_TIMEOUT_MS)).expect("max"),
            MAX_RELOAD_WAIT_TIMEOUT_MS
        );
        assert!(validate_reload_wait_timeout(Some(0)).is_err());
        assert!(validate_reload_wait_timeout(Some(MAX_RELOAD_WAIT_TIMEOUT_MS + 1)).is_err());
    }

    #[test]
    fn normal_bridge_attach_disabled_is_local_refusal() {
        let error = ChromeDebuggerBridgeError::normal_bridge_attach_disabled(1234, "snapshot");

        assert_eq!(
            error.code(),
            error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
        );
        assert_eq!(error.cdp_status(), CdpStatus::AttachFailed);
        assert!(
            error
                .detail()
                .contains("before queueing any Chrome command")
        );
        assert!(error.detail().contains("narrow chrome.debugger lanes"));
        assert!(error.detail().contains("viewportEmulation"));
        assert!(
            error
                .detail()
                .contains("dedicated raw-CDP automation profile")
        );
        assert!(
            error
                .detail()
                .contains("scripts\\install-synapse-chrome-debugger.ps1")
        );
        assert!(error.detail().contains("raw CDP"));
    }

    #[test]
    fn external_popup_risk_warning_blocks_until_suppressed() {
        let risks = vec![
            "profile=Profile 5 pref=Secure Preferences extension_id=fcoeoabgfenejglbffodgkkbkcdhcgfn name=\"Claude\" active_api=debugger,nativeMessaging".to_owned(),
            "native_messaging_process pid=26616 name=cmd.exe extension_id=fcoeoabgfenejglbffodgkkbkcdhcgfn".to_owned(),
        ];

        let warning = external_chrome_popup_risk_warning(&risks, false);

        assert!(warning.contains("external_chrome_popup_risk_blocking=true"));
        assert!(warning.contains("external_suppression_required"));
        assert!(warning.contains("risk_count=2"));
        assert!(warning.contains("fcoeoabgfenejglbffodgkkbkcdhcgfn"));
        assert!(warning.contains("fail closed"));

        let suppressed_warning = external_chrome_popup_risk_warning(&risks, true);
        assert!(suppressed_warning.contains("external_chrome_popup_risk_warning=true"));
        assert!(suppressed_warning.contains("covered_by_live_bridge_management"));
        assert!(suppressed_warning.contains("remaining_hazard_count=0"));
    }

    #[test]
    fn chrome_extension_runtime_state_treats_disabled_permission_rows_as_not_enabled() {
        let setting = json!({
            "state": 0,
            "active_bit": false,
            "disable_reasons": [65536],
            "active_permissions": {
                "api": ["downloads", "nativeMessaging"]
            }
        });

        let runtime_state = chrome_extension_runtime_state(&setting);

        assert_eq!(runtime_state.state, Some(0));
        assert_eq!(runtime_state.active_bit, Some(false));
        assert_eq!(runtime_state.disable_reasons, vec![65536]);
        assert!(!runtime_state.runtime_enabled);
    }

    #[test]
    fn chrome_extension_runtime_state_requires_enabled_state_even_with_active_permissions() {
        let setting = json!({
            "active_bit": false,
            "active_permissions": {
                "api": ["debugger", "nativeMessaging"]
            }
        });

        let runtime_state = chrome_extension_runtime_state(&setting);

        assert_eq!(runtime_state.state, None);
        assert_eq!(runtime_state.active_bit, Some(false));
        assert!(runtime_state.disable_reasons.is_empty());
        assert!(!runtime_state.runtime_enabled);
    }

    #[test]
    fn chrome_extension_runtime_state_treats_enabled_state_as_runtime_enabled() {
        let setting = json!({
            "state": 1,
            "active_bit": false,
            "active_permissions": {
                "api": ["debugger", "nativeMessaging"]
            }
        });

        let runtime_state = chrome_extension_runtime_state(&setting);

        assert_eq!(runtime_state.state, Some(1));
        assert_eq!(runtime_state.active_bit, Some(false));
        assert!(runtime_state.disable_reasons.is_empty());
        assert!(runtime_state.runtime_enabled);
    }

    #[test]
    fn chrome_extension_runtime_state_treats_absent_state_as_stale_not_enabled() {
        let setting = json!({
            "active_permissions": {
                "api": ["nativeMessaging"]
            }
        });

        let runtime_state = chrome_extension_runtime_state(&setting);

        assert_eq!(runtime_state.state, None);
        assert_eq!(runtime_state.active_bit, None);
        assert!(runtime_state.disable_reasons.is_empty());
        assert!(!runtime_state.runtime_enabled);
    }

    #[test]
    fn external_popup_risk_treats_active_manifest_absent_state_as_enabled_risk() {
        let setting = json!({
            "active_bit": false,
            "active_permissions": {
                "api": ["debugger", "nativeMessaging"]
            },
            "granted_permissions": {
                "api": ["debugger", "nativeMessaging"]
            },
            "manifest": {
                "permissions": ["debugger", "nativeMessaging"]
            }
        });
        let runtime_state = chrome_extension_runtime_state(&setting);
        let active_or_manifest_hazards = hazard_api_permissions(
            active_api_permissions(&setting)
                .iter()
                .chain(manifest_api_permissions(&setting).iter())
                .map(String::as_str),
        );
        let granted_hazards =
            hazard_api_permissions(granted_api_permissions(&setting).iter().map(String::as_str));

        assert_eq!(runtime_state.state, None);
        assert_eq!(runtime_state.active_bit, Some(false));
        assert!(runtime_state.disable_reasons.is_empty());
        assert!(!runtime_state.runtime_enabled);
        assert_eq!(
            active_or_manifest_hazards,
            vec!["debugger".to_owned(), "nativeMessaging".to_owned()]
        );
        assert!(external_popup_risk_enabled(
            &runtime_state,
            !active_or_manifest_hazards.is_empty(),
            !granted_hazards.is_empty()
        ));
    }

    #[test]
    fn external_popup_risk_ignores_absent_state_granted_only_residue() {
        let setting = json!({
            "active_permissions": {
                "api": ["alarms", "tabs"]
            },
            "granted_permissions": {
                "api": ["debugger", "nativeMessaging"]
            },
            "manifest": {
                "permissions": ["alarms", "tabs"]
            }
        });
        let runtime_state = chrome_extension_runtime_state(&setting);
        let active_or_manifest_hazards = hazard_api_permissions(
            active_api_permissions(&setting)
                .iter()
                .chain(manifest_api_permissions(&setting).iter())
                .map(String::as_str),
        );
        let granted_hazards =
            hazard_api_permissions(granted_api_permissions(&setting).iter().map(String::as_str));

        assert_eq!(runtime_state.state, None);
        assert!(active_or_manifest_hazards.is_empty());
        assert_eq!(
            granted_hazards,
            vec!["debugger".to_owned(), "nativeMessaging".to_owned()]
        );
        assert!(!external_popup_risk_enabled(
            &runtime_state,
            !active_or_manifest_hazards.is_empty(),
            !granted_hazards.is_empty()
        ));
    }

    #[test]
    fn external_popup_risk_respects_disable_reasons() {
        let setting = json!({
            "disable_reasons": [65536],
            "active_permissions": {
                "api": ["nativeMessaging"]
            },
            "manifest": {
                "permissions": ["nativeMessaging"]
            }
        });
        let runtime_state = chrome_extension_runtime_state(&setting);
        let active_or_manifest_hazards = hazard_api_permissions(
            active_api_permissions(&setting)
                .iter()
                .chain(manifest_api_permissions(&setting).iter())
                .map(String::as_str),
        );

        assert_eq!(runtime_state.disable_reasons, vec![65536]);
        assert!(!external_popup_risk_enabled(
            &runtime_state,
            !active_or_manifest_hazards.is_empty(),
            false
        ));
    }

    #[test]
    fn external_popup_risk_formatter_caps_noisy_readback() {
        let risks = (0..10)
            .map(|index| format!("risk-{index}"))
            .collect::<Vec<_>>();

        let formatted = format_external_chrome_popup_risks(&risks);

        assert!(formatted.contains("risk-0"));
        assert!(formatted.contains("risk-7"));
        assert!(!formatted.contains("risk-8 |"));
        assert!(formatted.ends_with("+2 more"));
    }
}
