//! CDP probe + attach for Chromium-family foregrounds.
//!
//! The diagnostic *types* ([`CdpDiagnostics`], [`CdpStatus`], [`CdpCapability`])
//! live in `synapse-core` because they are embedded in every `Observation`.
//! This module owns the *behaviour*: detecting a Chromium foreground, probing
//! for a reachable remote-debugging port, and attaching a `chromiumoxide`
//! client. It also owns the launched-port registry that ties
//! `act_launch` (#684) to the probe so a Synapse-launched browser is found
//! without the agent remembering manual flags.
//!
//! Background (research, 2026-06): since Chrome 136 the `--remote-debugging-port`
//! switch is ignored unless paired with a non-default `--user-data-dir`, so a
//! normally-launched Chrome on the user's primary profile can *never* expose a
//! debug port. That is why a normal launch probes `Unreachable` and why #684
//! must launch with a dedicated automation profile.

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    sync::{Mutex, OnceLock},
    time::Duration,
};

use synapse_core::error_codes;
use tokio::{net::TcpStream as TokioTcpStream, time::timeout};

pub use synapse_core::{CdpCapability, CdpDiagnostics, CdpStatus};

#[cfg(windows)]
use crate::{A11yError, A11yResult};

/// Default remote-debugging port probed when no launched port is registered for
/// the foreground process. 9222 is Chrome's conventional debug port.
pub const DEFAULT_CDP_PORT: u16 = 9222;

/// Environment override for the probed port list, e.g. `9222,9333`.
const CDP_PORTS_ENV: &str = "SYNAPSE_CDP_PORTS";
const CDP_UNREACHABLE_DETAIL: &str = "no reachable loopback CDP HTTP endpoint on checked ports; manual /json/version attach \
     requires Chrome/Edge to be started with --remote-debugging-port and, on Chrome 136+, \
     a non-default --user-data-dir. Chrome 144+ chrome://inspect/#remote-debugging is an \
     auto-connect permission flow, not a raw HTTP endpoint Synapse can probe through \
     SYNAPSE_CDP_PORTS";

#[must_use]
pub fn cdp_capabilities() -> Vec<CdpCapability> {
    vec![
        CdpCapability::DomSnapshot,
        CdpCapability::AccessibilityFullAxTree,
        CdpCapability::DomQuerySelector,
        CdpCapability::PageCaptureScreenshot,
        CdpCapability::PageFrameTree,
        CdpCapability::FlatIframeSessions,
        CdpCapability::PiercedShadowDom,
    ]
}

#[must_use]
pub fn is_chromium_family(process_name: &str) -> bool {
    let lower = process_name.to_ascii_lowercase();
    [
        "chrome.exe",
        "chromium.exe",
        "msedge.exe",
        "brave.exe",
        "vivaldi.exe",
        "opera.exe",
        "chrome",
        "chromium",
        "msedge",
        "brave",
        "vivaldi",
        "opera",
    ]
    .iter()
    .any(|candidate| lower.ends_with(candidate))
}

// === Launched-port registry =================================================
//
// `act_launch` registers a fully attested endpoint, not a bare port. The exact
// process generations and browser WebSocket identity make a stale row or port
// reuse distinguishable from the launched browser (#2166).

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchedCdpRegistration {
    pub registration_id: String,
    pub launch_pid: u32,
    pub launch_process_creation_time_100ns: Option<u64>,
    pub listener_pid: u32,
    pub listener_process_creation_time_100ns: Option<u64>,
    pub port: u16,
    pub browser_id: String,
    pub browser_websocket_url: String,
    pub user_data_dir_sha256: String,
}

/// Independent OS + HTTP readback for one loopback CDP listener. Launch code
/// uses this before publishing a registry row; probe code repeats the same
/// checks before trusting that row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdpListenerIdentity {
    pub listener_pid: u32,
    pub listener_process_creation_time_100ns: Option<u64>,
    pub browser_websocket_url: String,
}

/// Reads the physical owner of `port`, that process generation, and the browser
/// WebSocket identity advertised by `/json/version`. Exactly one IPv4 loopback
/// listener must own the port; ambiguity is a hard failure.
pub fn inspect_local_cdp_listener(
    port: u16,
    connect_timeout: Duration,
) -> Result<CdpListenerIdentity, String> {
    if port == 0 {
        return Err("refused to inspect invalid CDP listener port 0".to_owned());
    }
    #[cfg(windows)]
    {
        let listener_pids = tcp_listener_owner_pids(port)?;
        let [listener_pid] = listener_pids.as_slice() else {
            return Err(format!(
                "CDP port {port} must have exactly one IPv4 listener owner; actual_pids={listener_pids:?}"
            ));
        };
        let listener_process_creation_time_100ns =
            Some(process_creation_time_100ns(*listener_pid)?);
        let browser_websocket_url = fetch_browser_websocket_url(port, connect_timeout)?;
        Ok(CdpListenerIdentity {
            listener_pid: *listener_pid,
            listener_process_creation_time_100ns,
            browser_websocket_url,
        })
    }
    #[cfg(not(windows))]
    {
        let browser_websocket_url = fetch_browser_websocket_url(port, connect_timeout)?;
        Ok(CdpListenerIdentity {
            listener_pid: 0,
            listener_process_creation_time_100ns: None,
            browser_websocket_url,
        })
    }
}

/// Reads the creation identity of one local process generation. Windows is the
/// ownership authority for Synapse launch transactions; other platforms report
/// that this authority is unavailable instead of inventing an identity.
pub fn inspect_process_creation_time_100ns(pid: u32) -> Result<Option<u64>, String> {
    #[cfg(windows)]
    {
        process_creation_time_100ns(pid).map(Some)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        Err("process creation FILETIME authority is Windows-only".to_owned())
    }
}

fn registry() -> &'static Mutex<HashMap<u32, LaunchedCdpRegistration>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u32, LaunchedCdpRegistration>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Publishes an endpoint only after `act_launch` has attested every field.
pub fn register_launched_endpoint(registration: LaunchedCdpRegistration) -> Result<(), String> {
    let mut map = registry()
        .lock()
        .map_err(|_| "launched CDP registry lock poisoned".to_owned())?;
    if let Some(existing) = map.get(&registration.launch_pid)
        && existing != &registration
    {
        return Err(format!(
            "launched CDP registry already contains a contradictory row for pid {} existing_registration_id={} incoming_registration_id={}",
            registration.launch_pid, existing.registration_id, registration.registration_id
        ));
    }
    if let Some(existing) = map.values().find(|existing| {
        existing.registration_id != registration.registration_id
            && (existing.listener_pid == registration.listener_pid
                || existing.port == registration.port
                || existing.browser_websocket_url == registration.browser_websocket_url)
    }) {
        return Err(format!(
            "launched CDP registry endpoint identity is already owned: existing_launch_pid={} existing_registration_id={} incoming_launch_pid={} incoming_registration_id={} listener_pid={} port={}",
            existing.launch_pid,
            existing.registration_id,
            registration.launch_pid,
            registration.registration_id,
            registration.listener_pid,
            registration.port
        ));
    }
    tracing::info!(
        code = "A11Y_CDP_ENDPOINT_REGISTERED",
        pid = registration.launch_pid,
        listener_pid = registration.listener_pid,
        port = registration.port,
        registration_id = %registration.registration_id,
        browser_id = %registration.browser_id,
        "registered an identity-attested Synapse-launched CDP endpoint"
    );
    map.insert(registration.launch_pid, registration);
    Ok(())
}

/// Removes only the exact row owned by the caller. A PID-reused or replacement
/// registration is never evicted by stale cleanup.
pub fn forget_launched_endpoint(pid: u32, registration_id: &str) -> Result<bool, String> {
    let mut map = registry()
        .lock()
        .map_err(|_| "launched CDP registry lock poisoned".to_owned())?;
    let Some(existing) = map.get(&pid) else {
        return Ok(false);
    };
    if existing.registration_id != registration_id {
        return Err(format!(
            "refused to evict launched CDP pid {pid}: expected registration_id={registration_id} actual={}",
            existing.registration_id
        ));
    }
    map.remove(&pid);
    Ok(true)
}

/// The attested endpoint registered for `pid` by `act_launch`, if any.
pub fn launched_endpoint_for_pid(pid: u32) -> Result<Option<LaunchedCdpRegistration>, String> {
    registry()
        .lock()
        .map_err(|_| "launched CDP registry lock poisoned".to_owned())
        .map(|map| {
            map.get(&pid).cloned().or_else(|| {
                map.values()
                    .find(|registration| registration.listener_pid == pid)
                    .cloned()
            })
        })
}

/// The CDP debug port registered for `pid`, retained for diagnostics callers.
pub fn launched_port_for_pid(pid: u32) -> Result<Option<u16>, String> {
    launched_endpoint_for_pid(pid)
        .map(|registration| registration.map(|registration| registration.port))
}

/// Ports permitted for `pid`. An attested launch permits only its exact port;
/// configured/default ports are returned only when no launch row exists.
pub fn candidate_ports_for_pid(pid: u32) -> Result<Vec<u16>, String> {
    if let Some(port) = launched_port_for_pid(pid)? {
        return Ok(vec![port]);
    }
    Ok(configured_ports())
}

fn configured_ports() -> Vec<u16> {
    std::env::var(CDP_PORTS_ENV).map_or_else(
        |_| vec![DEFAULT_CDP_PORT],
        |raw| {
            let parsed: Vec<u16> = raw
                .split(',')
                .filter_map(|token| token.trim().parse::<u16>().ok())
                .filter(|port| *port != 0)
                .collect();
            if parsed.is_empty() {
                vec![DEFAULT_CDP_PORT]
            } else {
                parsed
            }
        },
    )
}

// === Probing ================================================================

fn endpoint_for_port(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn endpoints_for_ports(ports: &[u16]) -> Vec<String> {
    ports.iter().map(|port| endpoint_for_port(*port)).collect()
}

fn ok_diagnostics(process_name: &str, port: u16, checked_ports: Vec<u16>) -> CdpDiagnostics {
    CdpDiagnostics {
        process_name: process_name.to_owned(),
        status: CdpStatus::Ok,
        endpoint: Some(endpoint_for_port(port)),
        checked_endpoints: endpoints_for_ports(&checked_ports),
        checked_ports,
        reason_code: None,
        detail: None,
        capabilities: cdp_capabilities(),
        attached_node_count: None,
        selected_target_id: None,
        selected_session_id: None,
        target_selection_reason: None,
        target_candidate_count: None,
        frame_tree_frame_count: None,
        attached_frame_target_count: None,
        blocked_frame_targets: Vec::new(),
        frame_snapshot_errors: Vec::new(),
    }
}

/// Synchronous CDP reachability probe.
///
/// Used from the perception `platform_input` path so both `observe` and `find`
/// surface `cdp.status` without an async runtime. Connection-refused on loopback
/// returns immediately, so the common "no debug port" case costs microseconds,
/// not the full `connect_timeout`.
#[must_use]
pub fn probe_chromium_cdp_blocking(
    process_name: &str,
    ports: &[u16],
    connect_timeout: Duration,
) -> CdpDiagnostics {
    if !is_chromium_family(process_name) {
        return CdpDiagnostics::not_chromium(process_name);
    }
    let mut checked_ports = Vec::new();
    for port in ports {
        checked_ports.push(*port);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), *port);
        if TcpStream::connect_timeout(&addr, connect_timeout).is_ok() {
            return ok_diagnostics(process_name, *port, checked_ports);
        }
    }
    CdpDiagnostics::unreachable_with_probe(
        process_name,
        error_codes::A11Y_CDP_UNREACHABLE,
        checked_ports,
        CDP_UNREACHABLE_DETAIL,
    )
}

/// Probes CDP for a concrete process. A registered launch is an exact identity
/// contract: mismatch evicts that exact stale row and returns a typed failure;
/// it never tries configured/default ports as a substitute (#2166).
#[must_use]
pub fn probe_chromium_cdp_for_pid_blocking(
    process_name: &str,
    pid: u32,
    connect_timeout: Duration,
) -> CdpDiagnostics {
    let registration = match launched_endpoint_for_pid(pid) {
        Ok(Some(registration)) => registration,
        Ok(None) => {
            return probe_chromium_cdp_blocking(process_name, &configured_ports(), connect_timeout);
        }
        Err(detail) => {
            tracing::error!(
                code = error_codes::ACTION_LAUNCH_CDP_IDENTITY_MISMATCH,
                pid,
                detail = %detail,
                "launched CDP registry is unreadable; refusing configured/default port fallback"
            );
            return CdpDiagnostics::unreachable_with_probe(
                process_name,
                error_codes::ACTION_LAUNCH_CDP_IDENTITY_MISMATCH,
                Vec::new(),
                detail,
            );
        }
    };
    match validate_launched_registration_blocking(&registration, connect_timeout) {
        Ok(()) => ok_diagnostics(process_name, registration.port, vec![registration.port]),
        Err(detail) => {
            let eviction =
                forget_launched_endpoint(registration.launch_pid, &registration.registration_id);
            let detail = format!(
                "registered launched CDP identity mismatch: {detail}; exact_registry_eviction={eviction:?}; registration_id={}",
                registration.registration_id
            );
            tracing::error!(
                code = error_codes::ACTION_LAUNCH_CDP_IDENTITY_MISMATCH,
                pid,
                port = registration.port,
                registration_id = %registration.registration_id,
                detail = %detail,
                "rejected stale or contradictory launched CDP endpoint"
            );
            CdpDiagnostics::unreachable_with_probe(
                process_name,
                error_codes::ACTION_LAUNCH_CDP_IDENTITY_MISMATCH,
                vec![registration.port],
                detail,
            )
        }
    }
}

#[cfg(windows)]
fn validate_launched_registration_blocking(
    registration: &LaunchedCdpRegistration,
    connect_timeout: Duration,
) -> Result<(), String> {
    let launch_creation = process_creation_time_100ns(registration.launch_pid)?;
    if registration.launch_process_creation_time_100ns != Some(launch_creation) {
        return Err(format!(
            "launch process generation mismatch pid={} recorded={:?} actual={launch_creation}",
            registration.launch_pid, registration.launch_process_creation_time_100ns
        ));
    }
    let listener_creation = process_creation_time_100ns(registration.listener_pid)?;
    if registration.listener_process_creation_time_100ns != Some(listener_creation) {
        return Err(format!(
            "listener process generation mismatch pid={} recorded={:?} actual={listener_creation}",
            registration.listener_pid, registration.listener_process_creation_time_100ns
        ));
    }
    let listener_pids = tcp_listener_owner_pids(registration.port)?;
    if listener_pids != vec![registration.listener_pid] {
        return Err(format!(
            "listener ownership mismatch port={} recorded_pid={} actual_pids={listener_pids:?}",
            registration.port, registration.listener_pid
        ));
    }
    let actual_websocket = fetch_browser_websocket_url(registration.port, connect_timeout)?;
    if actual_websocket != registration.browser_websocket_url {
        return Err(format!(
            "browser websocket identity mismatch recorded={:?} actual={actual_websocket:?}",
            registration.browser_websocket_url
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn validate_launched_registration_blocking(
    registration: &LaunchedCdpRegistration,
    connect_timeout: Duration,
) -> Result<(), String> {
    let actual_websocket = fetch_browser_websocket_url(registration.port, connect_timeout)?;
    if actual_websocket != registration.browser_websocket_url {
        return Err(format!(
            "browser websocket identity mismatch recorded={:?} actual={actual_websocket:?}",
            registration.browser_websocket_url
        ));
    }
    Ok(())
}

fn fetch_browser_websocket_url(port: u16, timeout: Duration) -> Result<String, String> {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| format!("connect 127.0.0.1:{port}: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("set CDP read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("set CDP write timeout: {error}"))?;
    write!(
        stream,
        "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| format!("write CDP version request: {error}"))?;
    let mut bytes = Vec::new();
    stream
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read CDP version response: {error}"))?;
    if bytes.len() > 1024 * 1024 {
        return Err(format!(
            "CDP /json/version response exceeds 1048576-byte limit on port {port}"
        ));
    }
    let response = std::str::from_utf8(&bytes)
        .map_err(|error| format!("CDP version response is not UTF-8: {error}"))?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "CDP version response has no HTTP header terminator".to_owned())?;
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200 ") {
        return Err(format!("CDP version endpoint returned {status:?}"));
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("decode CDP /json/version: {error}"))?;
    value
        .get("webSocketDebuggerUrl")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| "CDP /json/version omitted webSocketDebuggerUrl".to_owned())
}

#[cfg(windows)]
fn process_creation_time_100ns(pid: u32) -> Result<u64, String> {
    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|error| format!("OpenProcess pid={pid}: {error}"))?;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let result = unsafe {
        GetProcessTimes(
            handle,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .map_err(|error| format!("GetProcessTimes pid={pid}: {error}"));
    let close =
        unsafe { CloseHandle(handle) }.map_err(|error| format!("CloseHandle pid={pid}: {error}"));
    result?;
    close?;
    let value = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    (value != 0)
        .then_some(value)
        .ok_or_else(|| format!("GetProcessTimes pid={pid} returned zero creation time"))
}

#[cfg(windows)]
fn tcp_listener_owner_pids(port: u16) -> Result<Vec<u32>, String> {
    use windows::Win32::{
        Foundation::ERROR_INSUFFICIENT_BUFFER,
        NetworkManagement::IpHelper::{
            GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
        },
        Networking::WinSock::AF_INET,
    };
    let mut byte_len = 0_u32;
    let first = unsafe {
        GetExtendedTcpTable(
            None,
            &raw mut byte_len,
            true,
            u32::from(AF_INET.0),
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if first != ERROR_INSUFFICIENT_BUFFER.0 || byte_len == 0 {
        return Err(format!(
            "GetExtendedTcpTable size query failed status={first} byte_len={byte_len}"
        ));
    }
    let word_len = usize::try_from(byte_len)
        .map_err(|_| format!("TCP table byte length does not fit usize: {byte_len}"))?
        .div_ceil(std::mem::size_of::<u32>());
    let mut storage = vec![0_u32; word_len];
    let status = unsafe {
        GetExtendedTcpTable(
            Some(storage.as_mut_ptr().cast()),
            &raw mut byte_len,
            true,
            u32::from(AF_INET.0),
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if status != 0 {
        return Err(format!(
            "GetExtendedTcpTable listener query failed status={status} byte_len={byte_len}"
        ));
    }
    let table = unsafe { &*storage.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>() };
    let count = usize::try_from(table.dwNumEntries).map_err(|_| {
        format!(
            "TCP listener count does not fit usize: {}",
            table.dwNumEntries
        )
    })?;
    let rows = unsafe { std::slice::from_raw_parts(table.table.as_ptr(), count) };
    let mut pids = Vec::new();
    for row in rows {
        let raw_port = u16::try_from(row.dwLocalPort).map_err(|_| {
            format!(
                "GetExtendedTcpTable returned out-of-range local port value {} for pid {}",
                row.dwLocalPort, row.dwOwningPid
            )
        })?;
        if u16::from_be(raw_port) != port {
            continue;
        }
        let local_address = Ipv4Addr::from(u32::from_be(row.dwLocalAddr));
        if !local_address.is_loopback() {
            return Err(format!(
                "CDP port {port} is not loopback-only: listener pid={} local_address={local_address}",
                row.dwOwningPid
            ));
        }
        pids.push(row.dwOwningPid);
    }
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

/// Async CDP reachability probe (used by tests and the async attach path).
pub async fn probe_chromium_cdp(
    process_name: &str,
    ports: &[u16],
    connect_timeout: Duration,
) -> CdpDiagnostics {
    if !is_chromium_family(process_name) {
        return CdpDiagnostics::not_chromium(process_name);
    }

    let mut checked_ports = Vec::new();
    for port in ports {
        checked_ports.push(*port);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), *port);
        if timeout(connect_timeout, TokioTcpStream::connect(addr))
            .await
            .is_ok_and(|result| result.is_ok())
        {
            return ok_diagnostics(process_name, *port, checked_ports);
        }
    }

    CdpDiagnostics::unreachable_with_probe(
        process_name,
        error_codes::A11Y_CDP_UNREACHABLE,
        checked_ports,
        CDP_UNREACHABLE_DETAIL,
    )
}

/// Resolves a reachable CDP endpoint for the browser window `hwnd`.
///
/// Used by action routing (#686). Looks up the window's pid, gathers its
/// candidate debug ports (launched-port registry first, then defaults), and
/// returns the first reachable `http://127.0.0.1:<port>`. `None` if the window
/// is gone, not a Chromium browser, or has no reachable debug port.
#[cfg(windows)]
#[must_use]
pub fn endpoint_for_window(hwnd: i64) -> Option<String> {
    let context = crate::foreground_context(hwnd).ok()?;
    probe_chromium_cdp_for_pid_blocking(
        &context.process_name,
        context.pid,
        Duration::from_millis(250),
    )
    .endpoint
}

#[cfg(windows)]
#[derive(Debug)]
pub struct CdpAttachment {
    pub browser: chromiumoxide::Browser,
    pub handler: chromiumoxide::Handler,
    pub endpoint: String,
}

/// Attaches a `chromiumoxide` browser client to a reachable CDP endpoint.
///
/// # Errors
///
/// Returns `A11Y_CDP_UNREACHABLE` when `chromiumoxide` cannot connect to the
/// supplied endpoint.
#[cfg(windows)]
pub async fn attach_chromiumoxide(endpoint: &str) -> A11yResult<CdpAttachment> {
    let (browser, handler) = chromiumoxide::Browser::connect(endpoint)
        .await
        .map_err(|err| A11yError::CdpUnreachable {
            detail: err.to_string(),
        })?;
    Ok(CdpAttachment {
        browser,
        handler,
        endpoint: endpoint.to_owned(),
    })
}

#[cfg(windows)]
#[derive(Clone, Debug)]
pub struct CdpTargetSummary {
    pub target_id: String,
    pub target_type: String,
    pub title: String,
    pub url: String,
    pub attached: bool,
}

#[cfg(windows)]
#[derive(Clone, Debug)]
pub struct CdpOpenTabResult {
    pub target: CdpTargetSummary,
    pub target_count_before: u32,
    pub target_count_after: u32,
}

#[cfg(windows)]
#[derive(Clone, Debug)]
pub struct CdpCloseTabResult {
    pub target_id: String,
    pub target_count_before: u32,
    pub target_count_after: u32,
}

#[cfg(windows)]
#[derive(Clone, Debug)]
pub struct CdpActivateTabResult {
    pub target_id: String,
    pub title: String,
    pub url: String,
}

/// Reads the current CDP `Target.getTargets` table for `endpoint`.
///
/// This is the physical Source of Truth for tab-target lifecycle checks; callers
/// should inspect it separately from any create/close return value.
///
/// # Errors
///
/// Returns `A11Y_CDP_UNREACHABLE` when the endpoint cannot be connected and
/// `A11Y_CDP_ATTACH_FAILED` when `Target.getTargets` itself fails.
#[cfg(windows)]
pub async fn cdp_list_targets(endpoint: &str) -> A11yResult<Vec<CdpTargetSummary>> {
    use futures_util::StreamExt as _;

    let CdpAttachment {
        browser,
        mut handler,
        ..
    } = attach_chromiumoxide(endpoint).await?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let result = cdp_list_targets_with_browser(&browser).await;
    handler_task.abort();
    result
}

/// Opens a visible background tab with `Target.createTarget(background=true)`,
/// then reads `Target.getTargets` until the returned target id is present.
///
/// # Errors
///
/// Returns fail-loud CDP errors when the endpoint is unreachable, the protocol
/// command fails, or the target does not appear in the target table.
#[cfg(windows)]
pub async fn cdp_open_background_tab(endpoint: &str, url: &str) -> A11yResult<CdpOpenTabResult> {
    use chromiumoxide::cdp::browser_protocol::target::CreateTargetParams;
    use futures_util::StreamExt as _;

    let CdpAttachment {
        browser,
        mut handler,
        ..
    } = attach_chromiumoxide(endpoint).await?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let before = cdp_list_targets_with_browser(&browser).await?;
        let params = CreateTargetParams::builder()
            .url(url)
            .new_window(false)
            .background(true)
            .build()
            .map_err(|error| A11yError::CdpAxtreeFailed {
                detail: format!("Target.createTarget params: {error}"),
            })?;
        let created =
            browser
                .execute(params)
                .await
                .map_err(|error| A11yError::CdpAxtreeFailed {
                    detail: format!("Target.createTarget(background=true): {error}"),
                })?;
        let target_id = created.result.target_id.inner().clone();
        let after = wait_for_target_present(&browser, &target_id).await?;
        let Some(target) = after
            .iter()
            .find(|target| target.target_id == target_id)
            .cloned()
        else {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "Target.createTarget returned {target_id:?}, but Target.getTargets readback did not contain it"
                ),
            });
        };
        Ok(CdpOpenTabResult {
            target,
            target_count_before: u32::try_from(before.len()).unwrap_or(u32::MAX),
            target_count_after: u32::try_from(after.len()).unwrap_or(u32::MAX),
        })
    }
    .await;

    handler_task.abort();
    result
}

/// Closes `target_id` with `Target.closeTarget`, then reads `Target.getTargets`
/// until the target is absent.
///
/// # Errors
///
/// Returns fail-loud CDP errors when the endpoint is unreachable, the target was
/// absent before close, the protocol command fails, or the target remains after
/// the close command.
#[cfg(windows)]
pub async fn cdp_close_target(endpoint: &str, target_id: &str) -> A11yResult<CdpCloseTabResult> {
    use chromiumoxide::cdp::browser_protocol::target::CloseTargetParams;
    use futures_util::StreamExt as _;

    let CdpAttachment {
        browser,
        mut handler,
        ..
    } = attach_chromiumoxide(endpoint).await?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let before = cdp_list_targets_with_browser(&browser).await?;
        if !before.iter().any(|target| target.target_id == target_id) {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "Target.closeTarget refused: Target.getTargets readback did not contain target_id {target_id:?} before close"
                ),
            });
        }
        browser
            .execute(CloseTargetParams::new(target_id.to_owned()))
            .await
            .map_err(|error| A11yError::CdpAxtreeFailed {
                detail: format!("Target.closeTarget({target_id:?}): {error}"),
            })?;
        let after = wait_for_target_absent(&browser, target_id).await?;
        Ok(CdpCloseTabResult {
            target_id: target_id.to_owned(),
            target_count_before: u32::try_from(before.len()).unwrap_or(u32::MAX),
            target_count_after: u32::try_from(after.len()).unwrap_or(u32::MAX),
        })
    }
    .await;

    handler_task.abort();
    result
}

/// Brings a raw-CDP page target to the front of its automation browser via
/// `Target.activateTarget` (the CDP-level analogue of selecting the tab). This
/// is for Synapse-launched automation profiles; it does not seize the human OS
/// foreground. Background-safe tab activation (#1189).
#[cfg(windows)]
pub async fn cdp_activate_target(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<CdpActivateTabResult> {
    use chromiumoxide::cdp::browser_protocol::target::ActivateTargetParams;
    use futures_util::StreamExt as _;

    let CdpAttachment {
        browser,
        mut handler,
        ..
    } = attach_chromiumoxide(endpoint).await?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let targets = cdp_list_targets_with_browser(&browser).await?;
        let Some(summary) = targets
            .into_iter()
            .find(|target| target.target_id == target_id)
        else {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "Target.activateTarget refused: Target.getTargets readback did not contain target_id {target_id:?}"
                ),
            });
        };
        browser
            .execute(ActivateTargetParams::new(target_id.to_owned()))
            .await
            .map_err(|error| A11yError::CdpAxtreeFailed {
                detail: format!("Target.activateTarget({target_id:?}): {error}"),
            })?;
        Ok(CdpActivateTabResult {
            target_id: target_id.to_owned(),
            title: summary.title,
            url: summary.url,
        })
    }
    .await;

    handler_task.abort();
    result
}

#[cfg(windows)]
async fn cdp_list_targets_with_browser(
    browser: &chromiumoxide::Browser,
) -> A11yResult<Vec<CdpTargetSummary>> {
    use chromiumoxide::cdp::browser_protocol::target::GetTargetsParams;

    let targets = browser
        .execute(GetTargetsParams::default())
        .await
        .map_err(|error| A11yError::CdpAttachFailed {
            detail: format!("Target.getTargets: {error}"),
        })?
        .result
        .target_infos
        .into_iter()
        .map(|target| CdpTargetSummary {
            target_id: target.target_id.inner().clone(),
            target_type: target.r#type,
            title: target.title,
            url: target.url,
            attached: target.attached,
        })
        .collect();
    Ok(targets)
}

#[cfg(windows)]
async fn wait_for_target_present(
    browser: &chromiumoxide::Browser,
    target_id: &str,
) -> A11yResult<Vec<CdpTargetSummary>> {
    let mut last = Vec::new();
    for _ in 0..30 {
        last = cdp_list_targets_with_browser(browser).await?;
        if last.iter().any(|target| target.target_id == target_id) {
            return Ok(last);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(A11yError::CdpAxtreeFailed {
        detail: format!(
            "target_id {target_id:?} did not appear in Target.getTargets within 3s; last target ids: {}",
            target_ids_for_error(&last)
        ),
    })
}

#[cfg(windows)]
async fn wait_for_target_absent(
    browser: &chromiumoxide::Browser,
    target_id: &str,
) -> A11yResult<Vec<CdpTargetSummary>> {
    let mut last = Vec::new();
    for _ in 0..30 {
        last = cdp_list_targets_with_browser(browser).await?;
        if !last.iter().any(|target| target.target_id == target_id) {
            return Ok(last);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(A11yError::CdpAxtreeFailed {
        detail: format!(
            "target_id {target_id:?} remained in Target.getTargets after close for 3s; last target ids: {}",
            target_ids_for_error(&last)
        ),
    })
}

#[cfg(windows)]
fn target_ids_for_error(targets: &[CdpTargetSummary]) -> String {
    targets
        .iter()
        .map(|target| target.target_id.as_str())
        .collect::<Vec<_>>()
        .join(",")
}
