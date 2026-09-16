//! Windows process-lifecycle coverage for the shared-daemon work. The legacy
//! `drop_kills_child.rs` / `sigint_clean_exit.rs` are `#![cfg(unix)]`, leaving
//! the exact platform where orphans accumulated untested. These exercise the
//! real binary (no mocks) on Windows: stdio servers exit on stdin EOF, and a
//! duplicate daemon is refused by the single-instance guard.

#![cfg(windows)]

use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synapse-mcp")
}

fn db_arg(dir: &Path) -> anyhow::Result<String> {
    dir.join("db")
        .to_str()
        .context("temp db path is not valid UTF-8")
        .map(ToOwned::to_owned)
}

fn free_loopback_bind() -> anyhow::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind ephemeral loopback port")?;
    let port = listener
        .local_addr()
        .context("read ephemeral local addr")?
        .port();
    Ok(format!("127.0.0.1:{port}"))
}

fn write_test_token_appdata(root: &Path) -> anyhow::Result<PathBuf> {
    let appdata = root.join("appdata");
    fs::create_dir_all(appdata.join("synapse"))?;
    fs::write(appdata.join("synapse").join("token.txt"), "test-token")?;
    Ok(appdata)
}

fn process_alive(pid: u32) -> bool {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    system.process(Pid::from_u32(pid)).is_some()
}

fn wait_for_pid_file(path: &Path, timeout: Duration) -> anyhow::Result<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(raw) = fs::read_to_string(path) {
            return raw
                .trim()
                .parse::<u32>()
                .with_context(|| format!("parse pid file {}", path.display()));
        }
        if Instant::now() > deadline {
            bail!("pid file did not appear: {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_pid_exit(pid: u32, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_alive(pid) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("pid {pid} did not exit within {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn kill_process(pid: u32) {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    if let Some(process) = system.process(Pid::from_u32(pid)) {
        let _ = process.kill();
    }
}

fn kill_synapse_processes_for_db(db: &str) {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    for process in system.processes().values() {
        if !process
            .name()
            .to_string_lossy()
            .to_lowercase()
            .contains("synapse-mcp")
        {
            continue;
        }
        let cmd = process
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        if cmd.contains(db) {
            let _ = process.kill();
        }
    }
}

fn windows_path_to_wsl(path: &str) -> anyhow::Result<String> {
    let mut chars = path.chars();
    let drive = chars
        .next()
        .context("Windows path missing drive letter")?
        .to_ascii_lowercase();
    if chars.next() != Some(':') {
        bail!("Windows path should start with drive colon: {path}");
    }
    let rest = chars.as_str().replace('\\', "/");
    Ok(format!("/mnt/{drive}{rest}"))
}

fn lifecycle_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn ps_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "''"))
}

/// A stdio server must exit promptly when its stdin reaches EOF (the parent
/// closing the pipe) — the shutdown path that was only tested on unix before.
#[test]
fn stdio_server_exits_on_stdin_eof() -> anyhow::Result<()> {
    let _guard = lifecycle_test_lock()
        .lock()
        .map_err(|err| anyhow::anyhow!("lifecycle test lock poisoned: {err}"))?;
    let tmp = tempfile::tempdir()?;
    let db = db_arg(tmp.path())?;
    let mut child = Command::new(bin())
        .args(["--mode", "stdio", "--db", &db, "--log-level", "warn"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn stdio server")?;

    std::thread::sleep(Duration::from_millis(800));
    // Close stdin -> EOF. The server must shut down.
    drop(child.stdin.take());

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().context("poll stdio server")?.is_some() {
            return Ok(());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!("stdio server did not exit within 15s after stdin EOF");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The connect bridge must exit when its parent process dies, even if stdin EOF
/// is not delivered. The daemon it connects to is intentionally cleaned up by
/// DB path after the bridge exit assertion.
#[test]
fn connect_bridge_exits_when_parent_process_dies() -> anyhow::Result<()> {
    let _guard = lifecycle_test_lock()
        .lock()
        .map_err(|err| anyhow::anyhow!("lifecycle test lock poisoned: {err}"))?;
    let tmp = tempfile::tempdir()?;
    let db = db_arg(tmp.path())?;
    let bind = free_loopback_bind()?;
    let appdata = write_test_token_appdata(tmp.path())?;

    let bridge_pid_file = tmp.path().join("bridge.pid");
    let parent_script = tmp.path().join("launch_bridge_parent.ps1");
    let script = format!(
        "$env:APPDATA = {}\n\
         $p = Start-Process -FilePath {} -ArgumentList @('--mode','connect','--bind',{},'--db',{},'--log-level','warn') -WindowStyle Hidden -PassThru\n\
         Set-Content -Path {} -Value $p.Id\n\
         Start-Sleep -Seconds 300\n",
        ps_quote(&appdata.display().to_string()),
        ps_quote(bin()),
        ps_quote(&bind),
        ps_quote(&db),
        ps_quote(&bridge_pid_file.display().to_string()),
    );
    fs::write(&parent_script, script)?;

    let mut parent = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            parent_script
                .to_str()
                .context("parent script path is not valid UTF-8")?,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn bridge parent process")?;

    let bridge_pid = match wait_for_pid_file(&bridge_pid_file, Duration::from_secs(20)) {
        Ok(pid) => pid,
        Err(error) => {
            let _ = parent.kill();
            let _ = parent.wait();
            kill_synapse_processes_for_db(&db);
            return Err(error);
        }
    };
    if !process_alive(bridge_pid) {
        let _ = parent.kill();
        let _ = parent.wait();
        kill_synapse_processes_for_db(&db);
        bail!("bridge pid {bridge_pid} exited before parent-death trigger");
    }

    parent.kill().context("kill bridge parent process")?;
    let _ = parent.wait();
    let exit_result = wait_for_pid_exit(bridge_pid, Duration::from_secs(20));
    if exit_result.is_err() {
        kill_process(bridge_pid);
    }
    kill_synapse_processes_for_db(&db);
    exit_result
}

/// The connect bridge must also exit when stdin reaches EOF before the client
/// sends a complete MCP initialize frame. This is the empty-input boundary that
/// can otherwise leave a bridge waiting forever under a still-alive parent.
#[test]
fn connect_bridge_exits_on_stdin_eof_before_init() -> anyhow::Result<()> {
    let _guard = lifecycle_test_lock()
        .lock()
        .map_err(|err| anyhow::anyhow!("lifecycle test lock poisoned: {err}"))?;
    let tmp = tempfile::tempdir()?;
    let db = db_arg(tmp.path())?;
    let bind = free_loopback_bind()?;
    let appdata = write_test_token_appdata(tmp.path())?;
    let mut child = Command::new(bin())
        .args([
            "--mode",
            "connect",
            "--bind",
            &bind,
            "--db",
            &db,
            "--log-level",
            "info",
        ])
        .env("APPDATA", &appdata)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn connect bridge")?;

    std::thread::sleep(Duration::from_millis(800));
    drop(child.stdin.take());

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().context("poll connect bridge")? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            kill_synapse_processes_for_db(&db);
            bail!("connect bridge did not exit within 20s after stdin EOF");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let output = child
        .wait_with_output()
        .context("collect connect bridge output")?;
    kill_synapse_processes_for_db(&db);

    assert!(status.success(), "connect bridge exit status: {status}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("MCP_CONNECT_EOF_CONNECTION_CLOSED"),
        "stderr should contain EOF guard log; stderr={stderr}"
    );
    assert!(
        stderr.contains("MCP_CONNECT_STDIN_EOF"),
        "stderr should contain bridge EOF shutdown log; stderr={stderr}"
    );
    Ok(())
}

/// A second daemon for the same DB must be refused by the single-instance guard
/// (exit code 3) while the first keeps running.
#[test]
fn duplicate_daemon_is_refused() -> anyhow::Result<()> {
    let _guard = lifecycle_test_lock()
        .lock()
        .map_err(|err| anyhow::anyhow!("lifecycle test lock poisoned: {err}"))?;
    let tmp = tempfile::tempdir()?;
    let db = db_arg(tmp.path())?;
    let pid_file = tmp.path().join("db").join("daemon.pid");
    let appdata = write_test_token_appdata(tmp.path())?;
    let first_bind = free_loopback_bind()?;
    let second_bind = free_loopback_bind()?;

    let mut first = Command::new(bin())
        .args([
            "--mode",
            "http",
            "--bind",
            &first_bind,
            "--db",
            &db,
            "--log-level",
            "warn",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("APPDATA", &appdata)
        .spawn()
        .context("spawn first daemon")?;

    // Wait until the first daemon has acquired the guard (records its PID).
    let deadline = Instant::now() + Duration::from_secs(20);
    while !pid_file.exists() {
        if let Some(status) = first.try_wait().context("poll first daemon startup")? {
            bail!("first daemon exited before acquiring lock: {status}");
        }
        if Instant::now() > deadline {
            let _ = first.kill();
            bail!("first daemon never acquired the single-instance lock");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Second daemon, same DB, different port -> guard refuses before bind.
    let second = Command::new(bin())
        .args([
            "--mode",
            "http",
            "--bind",
            &second_bind,
            "--db",
            &db,
            "--log-level",
            "warn",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("APPDATA", &appdata)
        .output()
        .context("run second daemon")?;

    let first_still_alive = first.try_wait().context("poll first daemon")?.is_none();
    let _ = first.kill();
    let _ = first.wait();

    assert_eq!(
        second.status.code(),
        Some(3),
        "duplicate daemon should exit 3 via the single-instance guard"
    );
    assert!(
        first_still_alive,
        "first daemon should survive the duplicate launch"
    );
    Ok(())
}

/// Direct WSL interop starts Windows children under the long-lived `wsl.exe`
/// host, so a Windows parent-death watchdog cannot prove the real Linux client
/// lifetime. This is ignored by default because it requires the operator's WSL
/// distro; run it on configured Windows hosts before accepting bridge lifecycle
/// changes.
#[test]
#[ignore = "requires configured Ubuntu-24.04 WSL distro"]
fn direct_wsl_interop_connect_parent_is_refused() -> anyhow::Result<()> {
    let _guard = lifecycle_test_lock()
        .lock()
        .map_err(|err| anyhow::anyhow!("lifecycle test lock poisoned: {err}"))?;
    let bind = free_loopback_bind()?;
    let wsl_bin = windows_path_to_wsl(bin())?;
    let output = Command::new("wsl.exe")
        .args([
            "-d",
            "Ubuntu-24.04",
            "-e",
            "env",
            "SYNAPSE_BEARER_TOKEN=lifecycle-wsl-parent-token",
            &wsl_bin,
            "--mode",
            "connect",
            "--bind",
            &bind,
            "--log-level",
            "info",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("spawn direct WSL interop connect bridge")?;

    assert!(
        !output.status.success(),
        "direct WSL interop bridge should be refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("MCP_CONNECT_UNSUPPORTED_PARENT"),
        "stderr should name unsupported parent; stderr={stderr}"
    );
    Ok(())
}
