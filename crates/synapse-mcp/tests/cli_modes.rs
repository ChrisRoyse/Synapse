use std::{net::TcpListener, process::Stdio, sync::OnceLock, time::Duration};

use anyhow::Context;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
};

#[tokio::test]
async fn http_mode_serves_health_until_shutdown() -> anyhow::Result<()> {
    let _guard = cli_mode_test_lock().lock().await;
    let dir = TempDir::new()?;
    let bind = free_loopback_bind()?;
    let token = "cli-mode-token";
    let mut child = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
        .args(["--mode", "http", "--bind", &bind])
        .env("SYNAPSE_LOG_DIR", dir.path())
        .env("APPDATA", dir.path())
        .env("LOCALAPPDATA", dir.path())
        .env("SYNAPSE_BEARER_TOKEN", token)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let response = wait_for_health(&bind, Some(token)).await;
    let missing = read_health_once(&bind, None).await;
    let wrong = read_health_once(&bind, Some("wrong-token")).await;
    let shutdown_missing = read_shutdown_once(&bind, None).await;
    let shutdown_wrong = read_shutdown_once(&bind, Some("wrong-token")).await;
    let pause_missing =
        read_maintenance_pause_once(&bind, None, "issue1410 endpoint auth", 120_000).await;
    let pause_wrong = read_maintenance_pause_once(
        &bind,
        Some("wrong-token"),
        "issue1410 endpoint auth",
        120_000,
    )
    .await;
    let pause_unavailable =
        read_maintenance_pause_once(&bind, Some(token), "issue1410 no active host", 120_000).await;
    let shutdown = read_shutdown_once(&bind, Some(token)).await;
    let mcp_during_drain = read_mcp_initialize_once(&bind, token).await;
    wait_child_exit(&mut child).await?;

    let response = response?;
    let missing = missing?;
    let wrong = wrong?;
    let shutdown_missing = shutdown_missing?;
    let shutdown_wrong = shutdown_wrong?;
    let pause_missing = pause_missing?;
    let pause_wrong = pause_wrong?;
    let pause_unavailable = pause_unavailable?;
    let shutdown = shutdown?;
    let mcp_during_drain = mcp_during_drain?;
    let logs = read_logs(dir.path())?;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains(r#""ok":true"#), "{response}");
    assert!(response.contains(r#""tool_count":40"#), "{response}");
    assert!(
        response
            .contains("public_tool_count=40 max_public_tool_count=40 implementation_tool_count="),
        "{response}"
    );
    assert!(!response.contains(r#""act_click""#), "{response}");
    assert!(
        missing.starts_with("HTTP/1.1 401 Unauthorized"),
        "{missing}"
    );
    assert!(wrong.starts_with("HTTP/1.1 401 Unauthorized"), "{wrong}");
    assert!(
        shutdown_missing.starts_with("HTTP/1.1 401 Unauthorized"),
        "{shutdown_missing}"
    );
    assert!(
        shutdown_wrong.starts_with("HTTP/1.1 401 Unauthorized"),
        "{shutdown_wrong}"
    );
    assert!(
        pause_missing.starts_with("HTTP/1.1 401 Unauthorized"),
        "{pause_missing}"
    );
    assert!(
        pause_wrong.starts_with("HTTP/1.1 401 Unauthorized"),
        "{pause_wrong}"
    );
    assert!(
        pause_unavailable.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{pause_unavailable}"
    );
    assert!(
        pause_unavailable
            .to_ascii_lowercase()
            .contains("connection: close"),
        "{pause_unavailable}"
    );
    assert!(
        pause_unavailable.contains("A11Y_CDP_EXTENSION_UNAVAILABLE"),
        "{pause_unavailable}"
    );
    assert!(shutdown.starts_with("HTTP/1.1 202 Accepted"), "{shutdown}");
    assert!(
        shutdown.to_ascii_lowercase().contains("connection: close"),
        "{shutdown}"
    );
    assert!(shutdown.contains(r#""shutdown":"requested""#), "{shutdown}");
    assert!(
        mcp_during_drain.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{mcp_during_drain}"
    );
    assert!(
        mcp_during_drain
            .to_ascii_lowercase()
            .contains("connection: close"),
        "{mcp_during_drain}"
    );
    assert!(
        mcp_during_drain.contains("DAEMON_RESTARTING"),
        "{mcp_during_drain}"
    );
    assert!(logs.contains("MCP_HTTP_SHUTDOWN_REQUESTED"), "{logs}");
    assert!(logs.contains("MCP_SHUTDOWN_GRACEFUL"), "{logs}");
    assert!(
        logs.contains("HTTP MCP request refused because daemon is draining"),
        "{logs}"
    );
    assert!(logs.contains("SAFETY_RELEASE_ALL_FIRED"), "{logs}");
    assert!(logs.contains("MCP_M2_EMITTER_SHUTDOWN_DONE"), "{logs}");
    Ok(())
}

#[tokio::test]
async fn http_shutdown_exits_with_open_keep_alive_client() -> anyhow::Result<()> {
    let _guard = cli_mode_test_lock().lock().await;
    let dir = TempDir::new()?;
    let bind = free_loopback_bind()?;
    let token = "cli-mode-keep-alive-token";
    let mut child = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
        .args(["--mode", "http", "--bind", &bind])
        .env("SYNAPSE_LOG_DIR", dir.path())
        .env("APPDATA", dir.path())
        .env("LOCALAPPDATA", dir.path())
        .env("SYNAPSE_BEARER_TOKEN", token)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    wait_for_health(&bind, Some(token)).await?;
    let mut keep_alive = open_keep_alive_health(&bind, token).await?;
    let shutdown = read_shutdown_once(&bind, Some(token)).await?;
    wait_child_exit(&mut child).await?;
    let rebound = TcpListener::bind(&bind)
        .with_context(|| format!("rebind {bind} after daemon shutdown with keep-alive client"))?;
    drop(rebound);

    let logs = read_logs(dir.path())?;
    assert!(shutdown.starts_with("HTTP/1.1 202 Accepted"), "{shutdown}");
    assert!(
        shutdown.to_ascii_lowercase().contains("connection: close"),
        "{shutdown}"
    );
    assert!(logs.contains("MCP_HTTP_SHUTDOWN_TOKEN_CANCELLED"), "{logs}");
    assert!(logs.contains("MCP_HTTP_CONNECTIONS_CANCELLED"), "{logs}");
    assert!(
        logs.contains("MCP_HTTP_SERVER_STOPPED") || logs.contains("MCP_HTTP_SERVER_ABORTED"),
        "{logs}"
    );

    let mut one = [0_u8; 1];
    let closed = tokio::time::timeout(Duration::from_secs(1), keep_alive.read(&mut one)).await;
    assert!(
        matches!(closed, Ok(Ok(0) | Err(_))),
        "keep-alive client still readable/open after daemon exit: {closed:?}"
    );
    Ok(())
}

#[tokio::test]
async fn http_shutdown_closes_active_mcp_sse_session() -> anyhow::Result<()> {
    let _guard = cli_mode_test_lock().lock().await;
    let dir = TempDir::new()?;
    let bind = free_loopback_bind()?;
    let token = "cli-mode-mcp-sse-token";
    let mut child = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
        .args(["--mode", "http", "--bind", &bind])
        .env("SYNAPSE_LOG_DIR", dir.path())
        .env("APPDATA", dir.path())
        .env("LOCALAPPDATA", dir.path())
        .env("SYNAPSE_BEARER_TOKEN", token)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    wait_for_health(&bind, Some(token)).await?;
    let initialize = read_mcp_initialize_once(&bind, token).await?;
    let session_id = header_value(&initialize, "mcp-session-id")
        .context("initialize returned no MCP session")?;
    let mut sse = open_mcp_sse_stream(&bind, token, &session_id).await?;
    let shutdown = read_shutdown_once(&bind, Some(token)).await?;
    wait_child_exit(&mut child).await?;
    let rebound = TcpListener::bind(&bind)
        .with_context(|| format!("rebind {bind} after daemon shutdown with active MCP SSE"))?;
    drop(rebound);

    let mut remaining = Vec::new();
    let closed =
        tokio::time::timeout(Duration::from_secs(2), sse.read_to_end(&mut remaining)).await;
    assert!(
        matches!(closed, Ok(Ok(_) | Err(_))),
        "MCP SSE client still open after daemon exit: {closed:?}"
    );

    let logs = read_logs(dir.path())?;
    assert!(shutdown.starts_with("HTTP/1.1 202 Accepted"), "{shutdown}");
    assert!(shutdown.contains(r#""sessions_before":1"#), "{shutdown}");
    assert!(shutdown.contains(r#""close_attempted":1"#), "{shutdown}");
    assert!(logs.contains("MCP_HTTP_SHUTDOWN_SESSIONS_CLOSED"), "{logs}");
    assert!(
        logs.contains("MCP_HTTP_SOCKET_SHUTDOWN_ON_DROP_ENABLED"),
        "{logs}"
    );
    if cfg!(windows) {
        assert!(
            logs.contains("MCP_HTTP_ACCEPTED_SOCKET_DROP_SHUTDOWN"),
            "{logs}"
        );
    }
    assert!(logs.contains(&session_id), "{logs}");
    Ok(())
}

#[tokio::test]
async fn http_mode_refuses_non_loopback_without_flag() -> anyhow::Result<()> {
    let _guard = cli_mode_test_lock().lock().await;
    let dir = TempDir::new()?;
    let output = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
        .args(["--mode", "http", "--bind", "0.0.0.0:0"])
        .env("SYNAPSE_LOG_DIR", dir.path())
        .env("APPDATA", dir.path())
        .env("LOCALAPPDATA", dir.path())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .await?;

    assert_eq!(output.status.code(), Some(2));
    let logs = read_logs(dir.path())?;
    assert!(logs.contains("HTTP_BIND_NON_LOOPBACK_REFUSED"), "{logs}");
    assert!(logs.contains("0.0.0.0:0"), "{logs}");
    Ok(())
}

#[tokio::test]
async fn http_mode_allows_non_loopback_with_explicit_flag() -> anyhow::Result<()> {
    let _guard = cli_mode_test_lock().lock().await;
    let dir = TempDir::new()?;
    let port = free_loopback_port()?;
    let bind = format!("0.0.0.0:{port}");
    let connect = format!("127.0.0.1:{port}");
    let token = "cli-mode-non-loopback-token";
    let mut child = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
        .args(["--mode", "http", "--bind", &bind, "--allow-non-loopback"])
        .env("SYNAPSE_LOG_DIR", dir.path())
        .env("APPDATA", dir.path())
        .env("LOCALAPPDATA", dir.path())
        .env("SYNAPSE_BEARER_TOKEN", token)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let response = wait_for_health_with_origin(&connect, Some(token), "http://127.0.0.1").await;
    stop_child(&mut child).await?;

    let response = response?;
    let logs = read_logs(dir.path())?;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(
        logs.contains("MCP_HTTP_NON_LOOPBACK_BIND_ALLOWED"),
        "{logs}"
    );
    assert!(logs.contains(&bind), "{logs}");
    Ok(())
}

#[tokio::test]
async fn stdio_mode_reaches_transport_path_on_closed_stdin() -> anyhow::Result<()> {
    let _guard = cli_mode_test_lock().lock().await;
    let dir = TempDir::new()?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
        .args(["--mode", "stdio"])
        .env("SYNAPSE_LOG_DIR", dir.path())
        .env("APPDATA", dir.path())
        .env("LOCALAPPDATA", dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .context("timed out waiting for stdio closed-stdin exit")??;
    assert!(status.success());

    let logs = read_logs(dir.path())?;
    assert!(logs.contains("MCP_STDIO_STARTED"));
    Ok(())
}

#[tokio::test]
async fn help_lists_m4_policy_flags_and_omits_removed_hardware_hid() -> anyhow::Result<()> {
    let _guard = cli_mode_test_lock().lock().await;
    let output = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
        .arg("--help")
        .env_remove("SYNAPSE_ALLOW_SHELL")
        .env_remove("SYNAPSE_ALLOW_LAUNCH")
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .await?;

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("--allow-shell <REGEX>"), "{stdout}");
    assert!(stdout.contains("SYNAPSE_ALLOW_SHELL"), "{stdout}");
    assert!(stdout.contains("--allow-launch <REGEX>"), "{stdout}");
    assert!(stdout.contains("SYNAPSE_ALLOW_LAUNCH"), "{stdout}");
    assert!(!stdout.contains("--hardware-hid"), "{stdout}");
    assert!(!stdout.contains("SYNAPSE_HARDWARE_HID"), "{stdout}");
    assert!(!stdout.contains("--reset-hardware-consent"), "{stdout}");
    Ok(())
}

#[tokio::test]
async fn http_health_reads_m4_policy_counts_from_repeated_cli_flags() -> anyhow::Result<()> {
    let _guard = cli_mode_test_lock().lock().await;
    let dir = TempDir::new()?;
    let bind = free_loopback_bind()?;
    let token = "cli-mode-m4-policy-cli-token";
    let mut child = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
        .args([
            "--mode",
            "http",
            "--bind",
            &bind,
            "--allow-shell",
            "^echo$",
            "--allow-shell",
            "^cargo$",
            "--allow-launch",
            "^notepad\\.exe$",
            "--allow-launch",
            "^calc\\.exe$",
            "--run-shell-inline-await-limit-ms",
            "1234",
        ])
        .env("SYNAPSE_LOG_DIR", dir.path())
        .env("APPDATA", dir.path())
        .env("LOCALAPPDATA", dir.path())
        .env("SYNAPSE_BEARER_TOKEN", token)
        // Opt out of the default-permissive shell/launch posture so health
        // reports the configured allowlist counts rather than "any".
        .env("SYNAPSE_ALLOW_SHELL_ANY", "0")
        .env("SYNAPSE_ALLOW_LAUNCH_ANY", "0")
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let response = wait_for_health(&bind, Some(token)).await;
    stop_child(&mut child).await?;

    let response = response?;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(
        response.contains("allow_shell_patterns=2 allow_launch_patterns=2"),
        "{response}"
    );
    assert!(
        response.contains("\"run_shell_inline_await_limit_ms\":1234"),
        "{response}"
    );
    assert!(
        response.contains("\"run_shell_inline_client_call_budget_ms\":110000"),
        "{response}"
    );
    assert!(
        response.contains("\"run_shell_durable_default_timeout_ms\":null"),
        "{response}"
    );
    assert!(
        response.contains("\"run_shell_durable_max_timeout_ms\":null"),
        "{response}"
    );
    Ok(())
}

#[tokio::test]
async fn http_health_reads_m4_policy_counts_from_comma_env() -> anyhow::Result<()> {
    let _guard = cli_mode_test_lock().lock().await;
    let dir = TempDir::new()?;
    let bind = free_loopback_bind()?;
    let token = "cli-mode-m4-policy-env-token";
    let mut child = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
        .args(["--mode", "http", "--bind", &bind])
        .env("SYNAPSE_LOG_DIR", dir.path())
        .env("APPDATA", dir.path())
        .env("LOCALAPPDATA", dir.path())
        .env("SYNAPSE_BEARER_TOKEN", token)
        .env("SYNAPSE_ALLOW_SHELL", "^echo$,^cargo$")
        .env("SYNAPSE_ALLOW_LAUNCH", "^notepad\\.exe$,^calc\\.exe$")
        // Opt out of the default-permissive shell/launch posture so health
        // reports the configured allowlist counts rather than "any".
        .env("SYNAPSE_ALLOW_SHELL_ANY", "0")
        .env("SYNAPSE_ALLOW_LAUNCH_ANY", "0")
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let response = wait_for_health(&bind, Some(token)).await;
    stop_child(&mut child).await?;

    let response = response?;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(
        response.contains("allow_shell_patterns=2 allow_launch_patterns=2"),
        "{response}"
    );
    Ok(())
}

#[tokio::test]
async fn invalid_env_mode_exits_with_clap_error() -> anyhow::Result<()> {
    let _guard = cli_mode_test_lock().lock().await;
    let output = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
        .env("SYNAPSE_MODE", "garbage")
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .await?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("invalid value") || stderr.contains("garbage"));
    Ok(())
}

#[tokio::test]
async fn invalid_max_subscriptions_env_exits_with_clap_error() -> anyhow::Result<()> {
    let _guard = cli_mode_test_lock().lock().await;
    let output = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
        .env("SYNAPSE_MAX_SUBSCRIPTIONS", "0")
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .await?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("max-subscriptions") || stderr.contains("SYNAPSE_MAX_SUBSCRIPTIONS"),
        "{stderr}"
    );
    assert!(stderr.contains('0'), "{stderr}");
    Ok(())
}

fn cli_mode_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn free_loopback_bind() -> anyhow::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    Ok(addr.to_string())
}

fn free_loopback_port() -> anyhow::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

async fn wait_for_health(bind: &str, token: Option<&str>) -> anyhow::Result<String> {
    wait_for_health_inner(bind, token, None).await
}

async fn wait_for_health_with_origin(
    bind: &str,
    token: Option<&str>,
    origin: &str,
) -> anyhow::Result<String> {
    wait_for_health_inner(bind, token, Some(origin)).await
}

async fn wait_for_health_inner(
    bind: &str,
    token: Option<&str>,
    origin: Option<&str>,
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match read_health_once_inner(bind, token, origin).await {
            Ok(response) => return Ok(response),
            Err(error) if tokio::time::Instant::now() < deadline => {
                let _last_error = error;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error).context("read HTTP health from child"),
        }
    }
}

async fn read_health_once(bind: &str, token: Option<&str>) -> anyhow::Result<String> {
    read_health_once_inner(bind, token, None).await
}

async fn read_health_once_inner(
    bind: &str,
    token: Option<&str>,
    origin: Option<&str>,
) -> anyhow::Result<String> {
    let mut stream = TcpStream::connect(bind).await?;
    let auth = token.map_or(String::new(), |token| {
        format!("Authorization: Bearer {token}\r\n")
    });
    let origin = origin.map_or(String::new(), |origin| format!("Origin: {origin}\r\n"));
    let request =
        format!("GET /health HTTP/1.1\r\nHost: {bind}\r\n{auth}{origin}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    String::from_utf8(response).context("decode HTTP health response")
}

async fn read_shutdown_once(bind: &str, token: Option<&str>) -> anyhow::Result<String> {
    let mut stream = TcpStream::connect(bind).await?;
    let auth = token.map_or(String::new(), |token| {
        format!("Authorization: Bearer {token}\r\n")
    });
    let request = format!(
        "POST /shutdown HTTP/1.1\r\nHost: {bind}\r\n{auth}User-Agent: synapse-cli-mode-test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    String::from_utf8(response).context("decode HTTP shutdown response")
}

async fn read_maintenance_pause_once(
    bind: &str,
    token: Option<&str>,
    reason: &str,
    pause_ms: u64,
) -> anyhow::Result<String> {
    let mut stream = TcpStream::connect(bind).await?;
    let auth = token.map_or(String::new(), |token| {
        format!("Authorization: Bearer {token}\r\n")
    });
    let body = format!(r#"{{"reason":"{reason}","pause_ms":{pause_ms}}}"#);
    let request = format!(
        "POST /chrome-debugger/native/maintenance-pause HTTP/1.1\r\nHost: {bind}\r\n{auth}User-Agent: synapse-cli-mode-test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    String::from_utf8(response).context("decode HTTP maintenance-pause response")
}

async fn read_mcp_initialize_once(bind: &str, token: &str) -> anyhow::Result<String> {
    let mut stream = TcpStream::connect(bind).await?;
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cli-mode-test","version":"0.0.0"}}}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {bind}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    String::from_utf8(response).context("decode HTTP MCP initialize response")
}

async fn open_keep_alive_health(bind: &str, token: &str) -> anyhow::Result<TcpStream> {
    let mut stream = TcpStream::connect(bind).await?;
    let request = format!(
        "GET /health HTTP/1.1\r\nHost: {bind}\r\nAuthorization: Bearer {token}\r\nConnection: keep-alive\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let response = read_single_http_response(&mut stream).await?;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(
        response.to_ascii_lowercase().contains("connection: close"),
        "{response}"
    );
    assert!(response.contains(r#""ok":true"#), "{response}");
    Ok(stream)
}

async fn open_mcp_sse_stream(
    bind: &str,
    token: &str,
    session_id: &str,
) -> anyhow::Result<TcpStream> {
    let mut stream = TcpStream::connect(bind).await?;
    let request = format!(
        "GET /mcp HTTP/1.1\r\nHost: {bind}\r\nAuthorization: Bearer {token}\r\nAccept: text/event-stream\r\nMcp-Session-Id: {session_id}\r\nConnection: keep-alive\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let response = read_http_headers(&mut stream).await?;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(
        response
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "{response}"
    );
    Ok(stream)
}

async fn read_http_headers(stream: &mut TcpStream) -> anyhow::Result<String> {
    let mut response = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .context("timed out reading HTTP response headers")??;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_header_end(&response) {
            response.truncate(end + 4);
            break;
        }
    }
    String::from_utf8(response).context("decode HTTP response headers")
}

async fn read_single_http_response(stream: &mut TcpStream) -> anyhow::Result<String> {
    let mut response = Vec::new();
    let mut header_end = None;
    let mut expected_len = None;
    let mut chunk = [0_u8; 1024];

    loop {
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .context("timed out reading HTTP response")??;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
        if header_end.is_none() {
            header_end = find_header_end(&response);
            if let Some(end) = header_end {
                let headers = String::from_utf8_lossy(&response[..end]);
                expected_len = content_length(&headers).map(|len| end + 4 + len);
            }
        }
        if expected_len.is_some_and(|len| response.len() >= len) {
            break;
        }
    }

    String::from_utf8(response).context("decode single HTTP response")
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    })
}

fn header_value(response: &str, name: &str) -> Option<String> {
    response
        .lines()
        .take_while(|line| !line.is_empty())
        .find_map(|line| {
            let (header, value) = line.split_once(':')?;
            header
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
}

async fn wait_child_exit(child: &mut Child) -> anyhow::Result<()> {
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .context("timed out waiting for http-mode child graceful shutdown")??;
    assert!(status.success(), "http-mode child exit status: {status}");
    Ok(())
}

async fn stop_child(child: &mut Child) -> anyhow::Result<()> {
    child.start_kill().context("stop http-mode child")?;
    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .context("timed out waiting for http-mode child shutdown")??;
    Ok(())
}

fn read_logs(path: &std::path::Path) -> anyhow::Result<String> {
    let mut logs = String::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.metadata()?.is_file() {
            logs.push_str(&std::fs::read_to_string(entry.path())?);
        }
    }
    Ok(logs)
}
