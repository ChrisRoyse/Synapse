use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, bail};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const SYNAPSE_SHELL_JOB_ROOT: &str = "SYNAPSE_SHELL_JOB_ROOT";
const TEST_M3_PERMISSIONS_NON_AUDIO: &str = concat!(
    "READ_EVENTS,WRITE_REFLEX,READ_REFLEX,READ_PROFILE,",
    "WRITE_PROFILE_ACTIVE,WRITE_REPLAY,READ_STORAGE,WRITE_STORAGE,",
    "INPUT_KEYBOARD,INPUT_MOUSE,INPUT_PAD"
);
const TEST_M3_PERMISSIONS_WITH_AUDIO: &str = concat!(
    "READ_EVENTS,WRITE_REFLEX,READ_REFLEX,READ_PROFILE,",
    "WRITE_PROFILE_ACTIVE,WRITE_REPLAY,READ_AUDIO,READ_STORAGE,WRITE_STORAGE,",
    "INPUT_KEYBOARD,INPUT_MOUSE,INPUT_PAD"
);

/// Minimal raw JSON-RPC stdio client for local MCP regression checks.
pub struct StdioMcpClient {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr_task: Option<tokio::task::JoinHandle<Vec<u8>>>,
    stderr_buffer: Arc<Mutex<Vec<u8>>>,
    _temp_db_dir: Option<tempfile::TempDir>,
    _temp_shell_job_root_dir: tempfile::TempDir,
    shell_job_root: PathBuf,
    next_id: u64,
    raw_rx: Vec<String>,
    raw_tx: Vec<String>,
}

impl StdioMcpClient {
    pub async fn launch_and_init() -> anyhow::Result<Self> {
        Self::launch_and_init_with_log_dir(None).await
    }

    pub async fn launch_and_init_with_log_dir(log_dir: Option<&Path>) -> anyhow::Result<Self> {
        Self::launch_with_env(log_dir, &[])?.initialize().await
    }

    pub async fn launch_and_init_with_env(
        log_dir: Option<&Path>,
        envs: &[(&str, &str)],
    ) -> anyhow::Result<Self> {
        Self::launch_with_env(log_dir, envs)?.initialize().await
    }

    async fn initialize(mut self) -> anyhow::Result<Self> {
        let init = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "synapse-test-utils",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        if init["serverInfo"]["name"] != "synapse-mcp" {
            bail!("unexpected initialize response: {init}");
        }
        self.notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(self)
    }

    pub fn launch(log_dir: Option<&Path>) -> anyhow::Result<Self> {
        Self::launch_with_env(log_dir, &[])
    }

    pub fn launch_with_env(log_dir: Option<&Path>, envs: &[(&str, &str)]) -> anyhow::Result<Self> {
        if envs
            .iter()
            .any(|(key, _value)| key.eq_ignore_ascii_case(SYNAPSE_SHELL_JOB_ROOT))
        {
            bail!(
                "StdioMcpClient owns {SYNAPSE_SHELL_JOB_ROOT} isolation; \
                 remove the caller-supplied value so each stdio test daemon gets a unique root"
            );
        }
        let bin = mcp_binary_path()?;
        let caller_supplied_db = envs
            .iter()
            .any(|(key, _value)| key.eq_ignore_ascii_case("SYNAPSE_DB"));
        let temp_db_dir = if caller_supplied_db {
            None
        } else {
            Some(
                tempfile::Builder::new()
                    .prefix("synapse-stdio-db-")
                    .tempdir()
                    .context("create unique SYNAPSE_DB temp directory for stdio MCP test daemon")?,
            )
        };
        let temp_shell_job_root_dir = tempfile::Builder::new()
            .prefix("synapse-stdio-shell-jobs-")
            .tempdir()
            .context(
                "create unique SYNAPSE_SHELL_JOB_ROOT temp directory for stdio MCP test daemon",
            )?;
        let shell_job_root = temp_shell_job_root_dir.path().to_path_buf();
        // Each stdio integration-test daemon owns a real, durable shell-job
        // root for its lifetime. This keeps the daemon's startup lock and any
        // durable job artifacts disjoint from the long-lived HTTP daemon and
        // from sibling tests running in parallel. Parent-process
        // SYNAPSE_SHELL_JOB_ROOT is intentionally ignored here; relying on a
        // shared process env var is the bug class fixed by #1648.
        if !shell_job_root.is_dir() {
            bail!(
                "unique {SYNAPSE_SHELL_JOB_ROOT} was not created as a directory: {}",
                shell_job_root.display()
            );
        }
        // Default a deterministic synthetic foreground for every stdio MCP test.
        //
        // Action-gated tools (reflex_register, act_spawn_agent, task_dispatch_once,
        // act_*, ...) run the supported-use scope gate, which reads the current
        // foreground via current_audit_foreground() -> GetForegroundWindow(). With
        // no fixture that calls the REAL desktop and hard-fails A11Y_NO_FOREGROUND
        // whenever the host has no focused window (locked screen, focus elsewhere,
        // unattended run). Because the live foreground fluctuates between and during
        // runs, this made `cargo test --workspace` intermittently red in a way that
        // could not be pinned to one test. Injecting the synthetic notepad fixture
        // here makes the foreground a controlled input for the whole suite — one
        // place, no per-test drift. Tests that deliberately exercise a different
        // perception state (their own fixture, or force-no-perception /
        // force-observe-internal) set one of these keys and keep full control; the
        // value lookup in current_audit_foreground checks the force flags first.
        let caller_controls_perception = envs.iter().any(|(key, _value)| {
            matches!(
                key.to_ascii_uppercase().as_str(),
                "SYNAPSE_MCP_SYNTHETIC_FIXTURE"
                    | "SYNAPSE_MCP_FORCE_NO_PERCEPTION"
                    | "SYNAPSE_MCP_FORCE_OBSERVE_INTERNAL"
                    | "SYNAPSE_MCP_FORCE_NO_FOREGROUND"
            )
        });
        let mut command = Command::new(bin);
        command
            .args(["--mode", "stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SYNAPSE_MCP_DISABLE_OPERATOR_HOTKEY", "1")
            .env("SYNAPSE_LOG_LEVEL", "debug");
        if !caller_controls_perception {
            command.env("SYNAPSE_MCP_SYNTHETIC_FIXTURE", "notepad");
        }
        let caller_supplied_permissions = envs
            .iter()
            .any(|(key, _value)| key.eq_ignore_ascii_case("SYNAPSE_MCP_ALLOWED_PERMISSIONS"));
        if !caller_supplied_permissions {
            command.env("SYNAPSE_MCP_ALLOWED_PERMISSIONS", test_m3_permissions(envs));
        }
        if let Some(temp_db_dir) = temp_db_dir.as_ref() {
            command.env("SYNAPSE_DB", temp_db_dir.path().join("db"));
        }
        if let Some(log_dir) = log_dir {
            command.env("SYNAPSE_LOG_DIR", log_dir);
        }
        for (key, value) in envs {
            command.env(key, value);
        }
        command.env(SYNAPSE_SHELL_JOB_ROOT, &shell_job_root);

        let mut child = command.spawn().context("spawn synapse-mcp")?;
        let stdin = child.stdin.take().context("child stdin missing")?;
        let stdout = child.stdout.take().context("child stdout missing")?;
        let stderr = child.stderr.take().context("child stderr missing")?;
        let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: BufReader::new(stdout).lines(),
            stderr_task: Some(tokio::spawn(read_stderr(
                stderr,
                Arc::clone(&stderr_buffer),
            ))),
            stderr_buffer,
            _temp_db_dir: temp_db_dir,
            _temp_shell_job_root_dir: temp_shell_job_root_dir,
            shell_job_root,
            next_id: 0,
            raw_rx: Vec::new(),
            raw_tx: Vec::new(),
        })
    }

    /// Tail of the daemon's stderr captured so far — the only diagnostics
    /// available when the child dies before answering a request.
    #[must_use]
    pub fn stderr_tail(&self, max_bytes: usize) -> String {
        let buffer = self
            .stderr_buffer
            .lock()
            .map(|buffer| buffer.clone())
            .unwrap_or_default();
        let start = buffer.len().saturating_sub(max_bytes);
        String::from_utf8_lossy(&buffer[start..]).into_owned()
    }

    pub async fn tools_list(&mut self) -> anyhow::Result<Value> {
        self.request("tools/list", serde_json::json!({})).await
    }

    pub async fn tools_call(&mut self, name: &str, args: Value) -> anyhow::Result<Value> {
        self.request(
            "tools/call",
            serde_json::json!({
                "name": name,
                "arguments": args,
            }),
        )
        .await
    }

    pub async fn tools_call_error(&mut self, name: &str, args: Value) -> anyhow::Result<Value> {
        self.request_error(
            "tools/call",
            serde_json::json!({
                "name": name,
                "arguments": args,
            }),
        )
        .await
    }

    pub async fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.next_id = self.next_id.saturating_add(1);
        let id = self.next_id;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_message(&request).await?;

        let response = tokio::time::timeout(REQUEST_TIMEOUT, self.read_response(id))
            .await
            .with_context(|| {
                format!("timed out waiting for JSON-RPC response to {method} id {id}")
            })??;
        if let Some(error) = response.get("error") {
            bail!("JSON-RPC error from {method}: {error}");
        }
        response
            .get("result")
            .cloned()
            .with_context(|| format!("JSON-RPC response missing result: {response}"))
    }

    pub async fn request_error(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.next_id = self.next_id.saturating_add(1);
        let id = self.next_id;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_message(&request).await?;

        let response = tokio::time::timeout(REQUEST_TIMEOUT, self.read_response(id))
            .await
            .with_context(|| {
                format!("timed out waiting for JSON-RPC error response to {method} id {id}")
            })??;
        response
            .get("error")
            .cloned()
            .with_context(|| format!("JSON-RPC response missing error: {response}"))
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&notification).await
    }

    pub async fn shutdown(mut self) -> anyhow::Result<std::process::ExitStatus> {
        drop(self.stdin.take());
        let mut child = self.child.take().context("child already reaped")?;
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .context("timed out waiting for child shutdown")?
            .context("wait for child")?;

        if let Some(stderr_task) = self.stderr_task.take() {
            let _stderr = stderr_task.await.context("join stderr reader")?;
        }
        Ok(status)
    }

    #[cfg(unix)]
    pub async fn send_sigint_and_wait(mut self) -> anyhow::Result<std::process::ExitStatus> {
        let pid = self.child_id().context("child pid missing")?;
        let kill_status = Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .status()
            .await
            .context("send SIGINT to child")?;
        if !kill_status.success() {
            bail!("kill -INT failed with status {kill_status}");
        }

        let mut child = self.child.take().context("child already reaped")?;
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .context("timed out waiting for child after SIGINT")?
            .context("wait for child after SIGINT")?;

        if let Some(stderr_task) = self.stderr_task.take() {
            let _stderr = stderr_task.await.context("join stderr reader")?;
        }
        Ok(status)
    }

    #[must_use]
    pub fn raw_received(&self) -> &[String] {
        &self.raw_rx
    }

    #[must_use]
    pub fn child_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    #[must_use]
    pub fn shell_job_root(&self) -> &Path {
        &self.shell_job_root
    }

    async fn write_message(&mut self, value: &Value) -> anyhow::Result<()> {
        let line = serde_json::to_string(value)?;
        self.raw_tx.push(line.clone());
        let stdin = self.stdin.as_mut().context("child stdin closed")?;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn read_response(&mut self, id: u64) -> anyhow::Result<Value> {
        while let Some(line) = self.stdout.next_line().await? {
            self.raw_rx.push(line.clone());
            let value: Value = serde_json::from_str(&line)
                .with_context(|| format!("parse JSON-RPC line: {line}"))?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(value);
            }
        }
        // Give the stderr reader a beat to drain the pipe before reporting.
        tokio::time::sleep(Duration::from_millis(100)).await;
        bail!(
            "MCP_TRANSPORT_CLOSED before response id {id}; daemon stderr tail:\n{}",
            self.stderr_tail(8 * 1024)
        );
    }
}

impl Drop for StdioMcpClient {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
            for _ in 0..20 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

fn test_m3_permissions(envs: &[(&str, &str)]) -> &'static str {
    let audio_enabled = envs.iter().rev().find_map(|(key, value)| {
        key.eq_ignore_ascii_case("SYNAPSE_ENABLE_AUDIO")
            .then_some(value.trim())
    });
    if matches!(audio_enabled, Some("1"))
        || audio_enabled.is_some_and(|value| value.eq_ignore_ascii_case("true"))
    {
        TEST_M3_PERMISSIONS_WITH_AUDIO
    } else {
        TEST_M3_PERMISSIONS_NON_AUDIO
    }
}

async fn read_stderr(mut stderr: ChildStderr, shared: Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    let mut buf = [0_u8; 4096];
    loop {
        match stderr.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if let Ok(mut shared) = shared.lock() {
                    shared.extend_from_slice(&buf[..read]);
                }
            }
        }
    }
    shared
        .lock()
        .map(|shared| shared.clone())
        .unwrap_or_default()
}

fn mcp_binary_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("SYNAPSE_MCP_BIN") {
        return Ok(PathBuf::from(path));
    }
    std::env::var_os("CARGO_BIN_EXE_synapse-mcp")
        .map(PathBuf::from)
        .context("CARGO_BIN_EXE_synapse-mcp is unset; run from synapse-mcp integration tests or set SYNAPSE_MCP_BIN")
}
