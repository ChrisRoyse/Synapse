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
// `act_launch` (#684) registers the ephemeral debug port it opened for a
// browser it launched, keyed by the browser's process id. The observe/find
// probe consults this registry first so a Synapse-launched browser is attached
// without any default-port collision or manual flag.

fn registry() -> &'static Mutex<HashMap<u32, u16>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u32, u16>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records the CDP debug `port` that `act_launch` opened for browser process
/// `pid`, so a later `observe`/`find` can find it.
pub fn register_launched_port(pid: u32, port: u16) {
    if let Ok(mut map) = registry().lock() {
        map.insert(pid, port);
        tracing::info!(
            code = "A11Y_CDP_PORT_REGISTERED",
            pid,
            port,
            "registered Synapse-launched CDP debug port"
        );
    }
}

/// Removes a registered port (e.g. when the browser process exits).
pub fn forget_launched_port(pid: u32) {
    if let Ok(mut map) = registry().lock() {
        map.remove(&pid);
    }
}

/// The CDP debug port registered for `pid` by `act_launch`, if any.
#[must_use]
pub fn launched_port_for_pid(pid: u32) -> Option<u16> {
    registry()
        .lock()
        .ok()
        .and_then(|map| map.get(&pid).copied())
}

/// The ordered list of ports to probe for `pid`: the registered launched port
/// (if any) first, then the env-configured / default port list. De-duplicated,
/// order-preserving.
#[must_use]
pub fn candidate_ports_for_pid(pid: u32) -> Vec<u16> {
    let mut ports = Vec::new();
    if let Some(port) = launched_port_for_pid(pid) {
        ports.push(port);
    }
    for port in configured_ports() {
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports
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
    let ports = candidate_ports_for_pid(context.pid);
    probe_chromium_cdp_blocking(&context.process_name, &ports, Duration::from_millis(250)).endpoint
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
