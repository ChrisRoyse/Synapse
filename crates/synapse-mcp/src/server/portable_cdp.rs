//! Minimal platform-neutral raw-CDP Runtime transport.
//!
//! The long-standing `synapse-a11y` action implementation also owns Windows
//! UIA/Win32 resources, so that module is intentionally Windows-gated. Browser
//! DOM readback on macOS only needs an exact, session-owned page WebSocket and
//! Runtime commands. Keeping that transport here prevents a target-scoped
//! operation from falling back to OS foreground input or to another tab.

use std::{net::IpAddr, time::Duration};

use futures_util::{SinkExt as _, StreamExt as _};
use rmcp::ErrorData;
use serde::Deserialize;
use serde_json::{Value, json};
use synapse_core::error_codes;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use crate::m1::mcp_error;

const CDP_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const CDP_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub(super) struct PortableCdpRuntimeResult {
    pub endpoint: String,
    pub target_id: String,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub result_type: String,
    pub result_subtype: Option<String>,
    pub value: Value,
    pub description: Option<String>,
    pub unserializable_value: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CdpTargetListEntry {
    id: String,
    #[serde(default, rename = "type")]
    target_type: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    web_socket_debugger_url: String,
}

pub(super) async fn runtime_evaluate(
    endpoint: &str,
    target_id: &str,
    expression: &str,
    await_promise: bool,
    return_by_value: bool,
    timeout_ms: u64,
) -> Result<PortableCdpRuntimeResult, ErrorData> {
    let (target, mut socket) = connect_exact_target(endpoint, target_id).await?;

    let expression_result = send_command(
        &mut socket,
        1,
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "awaitPromise": await_promise,
            "returnByValue": return_by_value,
            "userGesture": false,
            "objectGroup": "synapse_portable_runtime",
        }),
        Duration::from_millis(timeout_ms).min(Duration::from_secs(120)),
    )
    .await;
    let expression_result = match expression_result {
        Ok(result) => result,
        Err(error) => {
            log_release_failure(
                release_object_group(&mut socket, 2, "synapse_portable_runtime").await,
                "Runtime.evaluate command failure",
                target_id,
            );
            return Err(error);
        }
    };
    if let Some(exception) = expression_result.get("exceptionDetails") {
        log_release_failure(
            release_object_group(&mut socket, 2, "synapse_portable_runtime").await,
            "Runtime.evaluate exception",
            target_id,
        );
        return Err(portable_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            format!(
                "Runtime.evaluate threw in exact CDP target {target_id:?}: {}",
                compact_json(exception)
            ),
        ));
    }
    let remote = match expression_result.get("result") {
        Some(remote) => remote.clone(),
        None => {
            log_release_failure(
                release_object_group(&mut socket, 2, "synapse_portable_runtime").await,
                "Runtime.evaluate malformed result",
                target_id,
            );
            return Err(portable_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!(
                    "Runtime.evaluate response for exact CDP target {target_id:?} omitted result: {}",
                    compact_json(&expression_result)
                ),
            ));
        }
    };

    let page_state = read_page_state(&mut socket, 2, target_id).await;
    let release = release_object_group(&mut socket, 3, "synapse_portable_runtime").await;
    let (url, title, ready_state) = finish_page_state_and_release(
        page_state,
        release,
        "Runtime.evaluate page-state readback",
        target_id,
    )?;
    let result = PortableCdpRuntimeResult {
        endpoint: endpoint.to_owned(),
        target_id: target.id,
        url,
        title,
        ready_state,
        result_type: remote
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("undefined")
            .to_owned(),
        result_subtype: remote
            .get("subtype")
            .and_then(Value::as_str)
            .map(str::to_owned),
        value: remote.get("value").cloned().unwrap_or(Value::Null),
        description: remote
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        unserializable_value: remote
            .get("unserializableValue")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    let _ = tokio::time::timeout(Duration::from_millis(250), socket.close(None)).await;
    Ok(result)
}

#[expect(
    clippy::too_many_arguments,
    reason = "portable element evaluation mirrors the audited browser_evaluate contract"
)]
pub(super) async fn runtime_call_function_on(
    endpoint: &str,
    target_id: &str,
    backend_node_id: i64,
    function_declaration: &str,
    args: &[Value],
    await_promise: bool,
    return_by_value: bool,
    timeout_ms: u64,
) -> Result<PortableCdpRuntimeResult, ErrorData> {
    if backend_node_id <= 0 {
        return Err(portable_error(
            error_codes::ACTION_TARGET_INVALID,
            format!("backendNodeId must be positive; got {backend_node_id}"),
        ));
    }
    let (target, mut socket) = connect_exact_target(endpoint, target_id).await?;
    let object_group = "synapse_portable_element_runtime";
    let resolved = send_command(
        &mut socket,
        1,
        "DOM.resolveNode",
        json!({
            "backendNodeId": backend_node_id,
            "objectGroup": object_group,
        }),
        CDP_COMMAND_TIMEOUT,
    )
    .await?;
    let object_id = resolved
        .pointer("/object/objectId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    let object_id = match object_id {
        Some(object_id) => object_id,
        None => {
            log_release_failure(
                release_object_group(&mut socket, 2, object_group).await,
                "DOM.resolveNode malformed result",
                target_id,
            );
            return Err(portable_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "DOM.resolveNode for backendNodeId {backend_node_id} in exact target {target_id:?} returned no objectId: {}",
                    compact_json(&resolved)
                ),
            ));
        }
    };
    // Match the production Windows/browser contract: the resolved element is
    // the first function argument, followed by caller args. The wrapper also
    // supports arrow functions, which cannot bind `this` themselves.
    let wrapped = format!(
        "function() {{ return ({function_declaration}).apply(null, [this].concat(Array.prototype.slice.call(arguments))); }}"
    );
    let arguments = args
        .iter()
        .map(|value| json!({ "value": value }))
        .collect::<Vec<_>>();
    let call_result = send_command(
        &mut socket,
        2,
        "Runtime.callFunctionOn",
        json!({
            "functionDeclaration": wrapped,
            "objectId": object_id,
            "arguments": arguments,
            "awaitPromise": await_promise,
            "returnByValue": return_by_value,
            "userGesture": false,
            "objectGroup": object_group,
        }),
        Duration::from_millis(timeout_ms).min(Duration::from_secs(120)),
    )
    .await;
    let call_result = match call_result {
        Ok(result) => result,
        Err(error) => {
            log_release_failure(
                release_object_group(&mut socket, 3, object_group).await,
                "Runtime.callFunctionOn command failure",
                target_id,
            );
            return Err(error);
        }
    };
    if let Some(exception) = call_result.get("exceptionDetails") {
        log_release_failure(
            release_object_group(&mut socket, 3, object_group).await,
            "Runtime.callFunctionOn exception",
            target_id,
        );
        return Err(portable_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            format!(
                "Runtime.callFunctionOn threw for backendNodeId {backend_node_id} in exact target {target_id:?}: {}",
                compact_json(exception)
            ),
        ));
    }
    let remote = match call_result.get("result") {
        Some(remote) => remote.clone(),
        None => {
            log_release_failure(
                release_object_group(&mut socket, 3, object_group).await,
                "Runtime.callFunctionOn malformed result",
                target_id,
            );
            return Err(portable_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!(
                    "Runtime.callFunctionOn response for backendNodeId {backend_node_id} in exact target {target_id:?} omitted result: {}",
                    compact_json(&call_result)
                ),
            ));
        }
    };
    let page_state = read_page_state(&mut socket, 3, target_id).await;
    let release = release_object_group(&mut socket, 4, object_group).await;
    let (url, title, ready_state) = finish_page_state_and_release(
        page_state,
        release,
        "Runtime.callFunctionOn page-state readback",
        target_id,
    )?;
    let result = PortableCdpRuntimeResult {
        endpoint: endpoint.to_owned(),
        target_id: target.id,
        url,
        title,
        ready_state,
        result_type: remote
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("undefined")
            .to_owned(),
        result_subtype: remote
            .get("subtype")
            .and_then(Value::as_str)
            .map(str::to_owned),
        value: remote.get("value").cloned().unwrap_or(Value::Null),
        description: remote
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        unserializable_value: remote
            .get("unserializableValue")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    let _ = tokio::time::timeout(Duration::from_millis(250), socket.close(None)).await;
    Ok(result)
}

async fn connect_exact_target(
    endpoint: &str,
    target_id: &str,
) -> Result<
    (
        CdpTargetListEntry,
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    ),
    ErrorData,
> {
    validate_loopback_endpoint(endpoint)?;
    let target = discover_exact_target(endpoint, target_id).await?;
    validate_loopback_websocket(&target.web_socket_debugger_url)?;
    let connect = tokio_tungstenite::connect_async(target.web_socket_debugger_url.as_str());
    let (socket, _response) = tokio::time::timeout(CDP_DISCOVERY_TIMEOUT, connect)
        .await
        .map_err(|_| {
            portable_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!(
                    "exact CDP target {target_id:?} WebSocket connect exceeded {} ms at {}",
                    CDP_DISCOVERY_TIMEOUT.as_millis(),
                    target.web_socket_debugger_url
                ),
            )
        })?
        .map_err(|error| {
            portable_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!(
                    "exact CDP target {target_id:?} WebSocket connect failed at {}: {error}",
                    target.web_socket_debugger_url
                ),
            )
        })?;
    Ok((target, socket))
}

async fn read_page_state(
    socket: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    id: u64,
    target_id: &str,
) -> Result<(String, String, String), ErrorData> {
    // This independent command is a same-socket, same-target identity
    // correlation. It is never inferred from discovery or mutation output.
    let page_state_result = send_command(
        socket,
        id,
        "Runtime.evaluate",
        json!({
            "expression": "({url:String(location.href||''),title:String(document.title||''),ready_state:String(document.readyState||'')})",
            "awaitPromise": false,
            "returnByValue": true,
            "userGesture": false,
        }),
        CDP_COMMAND_TIMEOUT,
    )
    .await?;
    if let Some(exception) = page_state_result.get("exceptionDetails") {
        return Err(portable_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            format!(
                "same-target page-state Runtime.evaluate threw for {target_id:?}: {}",
                compact_json(exception)
            ),
        ));
    }
    let page_state = page_state_result
        .pointer("/result/value")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            portable_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!(
                    "same-target page-state readback for {target_id:?} was not an object: {}",
                    compact_json(&page_state_result)
                ),
            )
        })?;
    Ok((
        required_string(page_state, "url", target_id)?,
        required_string(page_state, "title", target_id)?,
        required_string(page_state, "ready_state", target_id)?,
    ))
}

async fn release_object_group(
    socket: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    id: u64,
    object_group: &str,
) -> Result<(), ErrorData> {
    send_command(
        socket,
        id,
        "Runtime.releaseObjectGroup",
        json!({ "objectGroup": object_group }),
        CDP_COMMAND_TIMEOUT,
    )
    .await
    .map(|_| ())
}

fn log_release_failure(result: Result<(), ErrorData>, phase: &str, target_id: &str) {
    if let Err(error) = result {
        tracing::error!(
            code = "PORTABLE_CDP_OBJECT_GROUP_RELEASE_FAILED",
            phase,
            target_id,
            error = %error,
            "portable raw-CDP object-group cleanup failed after primary command failure"
        );
    }
}

fn finish_page_state_and_release(
    page_state: Result<(String, String, String), ErrorData>,
    release: Result<(), ErrorData>,
    phase: &str,
    target_id: &str,
) -> Result<(String, String, String), ErrorData> {
    match (page_state, release) {
        (Ok(state), Ok(())) => Ok(state),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(release)) => Err(release),
        (Err(primary), Err(release)) => {
            tracing::error!(
                code = "PORTABLE_CDP_OBJECT_GROUP_RELEASE_FAILED",
                phase,
                target_id,
                error = %release,
                "portable raw-CDP object-group cleanup also failed after page-state readback failure"
            );
            Err(primary)
        }
    }
}

async fn discover_exact_target(
    endpoint: &str,
    target_id: &str,
) -> Result<CdpTargetListEntry, ErrorData> {
    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(portable_error(
            error_codes::TOOL_PARAMS_INVALID,
            "portable CDP target id must not be empty",
        ));
    }
    let url = format!("{}/json/list", endpoint.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(CDP_DISCOVERY_TIMEOUT)
        .build()
        .map_err(|error| {
            portable_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!("build bounded CDP discovery client failed: {error}"),
            )
        })?;
    let response = client.get(&url).send().await.map_err(|error| {
        portable_error(
            error_codes::A11Y_CDP_UNREACHABLE,
            format!("read exact CDP target list from {url} failed: {error}"),
        )
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(portable_error(
            error_codes::A11Y_CDP_UNREACHABLE,
            format!("CDP target list {url} returned HTTP {status}"),
        ));
    }
    let targets = response
        .json::<Vec<CdpTargetListEntry>>()
        .await
        .map_err(|error| {
            portable_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!("decode CDP target list {url} failed: {error}"),
            )
        })?;
    let matching = targets
        .into_iter()
        .filter(|candidate| candidate.id.eq_ignore_ascii_case(target_id))
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(portable_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "CDP /json/list resolved target id {target_id:?} {} times; exact unique target required",
                matching.len()
            ),
        ));
    }
    let target = matching.into_iter().next().ok_or_else(|| {
        portable_error(
            error_codes::ACTION_TARGET_INVALID,
            format!("exact CDP target {target_id:?} disappeared during discovery"),
        )
    })?;
    if target.target_type != "page" || target.web_socket_debugger_url.trim().is_empty() {
        return Err(portable_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "CDP target {target_id:?} is not an attachable page: type={:?} url={:?} title={:?}",
                target.target_type, target.url, target.title
            ),
        ));
    }
    Ok(target)
}

async fn send_command(
    socket: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    id: u64,
    method: &str,
    params: Value,
    budget: Duration,
) -> Result<Value, ErrorData> {
    let payload = serde_json::to_string(&json!({
        "id": id,
        "method": method,
        "params": params,
    }))
    .map_err(|error| {
        portable_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("serialize CDP command {method} id={id} failed: {error}"),
        )
    })?;
    tokio::time::timeout(budget, socket.send(Message::Text(payload.into())))
        .await
        .map_err(|_| {
            portable_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!(
                    "CDP command {method} id={id} send exceeded {} ms",
                    budget.as_millis()
                ),
            )
        })?
        .map_err(|error| {
            portable_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!("CDP command {method} id={id} send failed: {error}"),
            )
        })?;
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(portable_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!(
                    "CDP command {method} id={id} acknowledgement exceeded {} ms",
                    budget.as_millis()
                ),
            ));
        }
        let message = tokio::time::timeout(remaining, socket.next())
            .await
            .map_err(|_| {
                portable_error(
                    error_codes::A11Y_CDP_UNREACHABLE,
                    format!(
                        "CDP command {method} id={id} acknowledgement exceeded {} ms",
                        budget.as_millis()
                    ),
                )
            })?
            .ok_or_else(|| {
                portable_error(
                    error_codes::A11Y_CDP_UNREACHABLE,
                    format!("CDP socket closed before {method} id={id} acknowledgement"),
                )
            })?
            .map_err(|error| {
                portable_error(
                    error_codes::A11Y_CDP_UNREACHABLE,
                    format!("read CDP {method} id={id} acknowledgement failed: {error}"),
                )
            })?;
        let decoded = match message {
            Message::Text(text) => serde_json::from_str::<Value>(text.as_str()),
            Message::Binary(bytes) => serde_json::from_slice::<Value>(&bytes),
            Message::Ping(payload) => {
                socket.send(Message::Pong(payload)).await.map_err(|error| {
                    portable_error(
                        error_codes::A11Y_CDP_UNREACHABLE,
                        format!("CDP pong failed while awaiting {method} id={id}: {error}"),
                    )
                })?;
                continue;
            }
            Message::Close(frame) => {
                return Err(portable_error(
                    error_codes::A11Y_CDP_UNREACHABLE,
                    format!("CDP socket closed before {method} id={id} acknowledgement: {frame:?}"),
                ));
            }
            Message::Pong(_) | Message::Frame(_) => continue,
        }
        .map_err(|error| {
            portable_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!("decode CDP response while awaiting {method} id={id} failed: {error}"),
            )
        })?;
        if decoded.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = decoded.get("error") {
            return Err(portable_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!(
                    "Chrome rejected CDP command {method} id={id}: {}",
                    compact_json(error)
                ),
            ));
        }
        return decoded.get("result").cloned().ok_or_else(|| {
            portable_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!(
                    "CDP command {method} id={id} response omitted result/error: {}",
                    compact_json(&decoded)
                ),
            )
        });
    }
}

fn validate_loopback_endpoint(endpoint: &str) -> Result<(), ErrorData> {
    validate_loopback_url(endpoint, &["http", "https"], "CDP endpoint")
}

fn validate_loopback_websocket(url: &str) -> Result<(), ErrorData> {
    validate_loopback_url(url, &["ws", "wss"], "CDP page WebSocket")
}

fn validate_loopback_url(url: &str, schemes: &[&str], label: &str) -> Result<(), ErrorData> {
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        portable_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{label} URL {url:?} is invalid: {error}"),
        )
    })?;
    if !schemes.contains(&parsed.scheme()) {
        return Err(portable_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{label} URL must use one of {schemes:?}; got {url:?}"),
        ));
    }
    let host = parsed.host_str().ok_or_else(|| {
        portable_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{label} URL {url:?} has no host"),
        )
    })?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !loopback {
        return Err(portable_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "{label} URL host {host:?} is not loopback; refusing remote browser attachment"
            ),
        ));
    }
    Ok(())
}

fn required_string(
    object: &serde_json::Map<String, Value>,
    field: &str,
    target_id: &str,
) -> Result<String, ErrorData> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            portable_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!("same-target page-state readback for {target_id:?} omitted string {field}"),
            )
        })
}

fn compact_json(value: &Value) -> String {
    value.to_string().chars().take(2_048).collect()
}

fn portable_error(code: &'static str, detail: impl Into<String>) -> ErrorData {
    mcp_error(code, format!("portable raw CDP: {}", detail.into()))
}
