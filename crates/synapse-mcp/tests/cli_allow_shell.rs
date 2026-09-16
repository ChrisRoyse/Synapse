use std::{process::Stdio, sync::OnceLock};

use tempfile::TempDir;
use tokio::process::Command;

#[tokio::test]
async fn allow_shell_rejects_broad_patterns_at_startup() -> anyhow::Result<()> {
    let _guard = cli_allow_test_lock().lock().await;
    let cases = [
        (".*", "unbounded_any_character_repetition"),
        ("^.+$", "unbounded_any_character_repetition"),
        ("", "empty_pattern"),
        ("git status", "shell_pattern_must_match_full_command_line"),
        ("^$", "matches_empty"),
    ];

    for (pattern, reason) in cases {
        let dir = TempDir::new()?;
        let output = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
            .args(["--mode", "stdio", "--allow-shell", pattern])
            .env("SYNAPSE_LOG_DIR", dir.path())
            .env_remove("SYNAPSE_ALLOW_SHELL")
            .env_remove("SYNAPSE_ALLOW_LAUNCH")
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .output()
            .await?;

        assert_eq!(
            output.status.code(),
            Some(2),
            "pattern {pattern:?} should fail startup"
        );
        let stderr = String::from_utf8(output.stderr)?;
        let logs = read_logs(dir.path())?;
        let combined = format!("{stderr}\n{logs}");
        assert!(combined.contains("CONFIG_INVALID"), "{combined}");
        assert!(combined.contains("SHELL_PATTERN_TOO_BROAD"), "{combined}");
        assert!(combined.contains("SYNAPSE_ALLOW_SHELL"), "{combined}");
        assert!(combined.contains(reason), "{combined}");
        assert!(!combined.contains("MCP_STDIO_STARTED"), "{combined}");
    }

    Ok(())
}

#[tokio::test]
async fn allow_launch_rejects_broad_patterns_at_startup() -> anyhow::Result<()> {
    let _guard = cli_allow_test_lock().lock().await;
    let cases = [
        (".*", "unbounded_any_character_repetition"),
        ("^.+$", "unbounded_any_character_repetition"),
        ("", "empty_pattern"),
        ("notepad.exe", "launch_pattern_must_match_full_command_line"),
        ("^$", "matches_empty"),
    ];

    for (pattern, reason) in cases {
        let dir = TempDir::new()?;
        let output = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
            .args(["--mode", "stdio", "--allow-launch", pattern])
            .env("SYNAPSE_LOG_DIR", dir.path())
            .env_remove("SYNAPSE_ALLOW_SHELL")
            .env_remove("SYNAPSE_ALLOW_LAUNCH")
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .output()
            .await?;

        assert_eq!(
            output.status.code(),
            Some(2),
            "pattern {pattern:?} should fail startup"
        );
        let stderr = String::from_utf8(output.stderr)?;
        let logs = read_logs(dir.path())?;
        let combined = format!("{stderr}\n{logs}");
        assert!(combined.contains("CONFIG_INVALID"), "{combined}");
        assert!(combined.contains("LAUNCH_PATTERN_TOO_BROAD"), "{combined}");
        assert!(combined.contains("SYNAPSE_ALLOW_LAUNCH"), "{combined}");
        assert!(combined.contains(reason), "{combined}");
        assert!(!combined.contains("MCP_STDIO_STARTED"), "{combined}");
    }

    Ok(())
}

#[tokio::test]
async fn allow_shell_accepts_narrow_patterns_and_reaches_stdio() -> anyhow::Result<()> {
    let _guard = cli_allow_test_lock().lock().await;
    let cases = [
        r"^git \w+$",
        r"^echo .{0,100}$",
        r"^cargo (build|test)( --[\w-]+)*$",
    ];

    for pattern in cases {
        let dir = TempDir::new()?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
            .args(["--mode", "stdio", "--allow-shell", pattern])
            .env("SYNAPSE_LOG_DIR", dir.path())
            .env_remove("SYNAPSE_ALLOW_SHELL")
            .env_remove("SYNAPSE_ALLOW_LAUNCH")
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let status =
            tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await??;
        assert!(status.success(), "pattern {pattern:?} should allow startup");
        let logs = read_logs(dir.path())?;
        assert!(logs.contains("MCP_STDIO_STARTED"), "{logs}");
    }

    Ok(())
}

#[tokio::test]
async fn allow_launch_accepts_narrow_patterns_and_reaches_stdio() -> anyhow::Result<()> {
    let _guard = cli_allow_test_lock().lock().await;
    let cases = [
        r"^notepad\.exe$",
        r"^javaw\.exe$",
        r"^C:\\Program Files\\Microsoft VS Code\\Code\.exe$",
    ];

    for pattern in cases {
        let dir = TempDir::new()?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_synapse-mcp"))
            .args(["--mode", "stdio", "--allow-launch", pattern])
            .env("SYNAPSE_LOG_DIR", dir.path())
            .env_remove("SYNAPSE_ALLOW_SHELL")
            .env_remove("SYNAPSE_ALLOW_LAUNCH")
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let status =
            tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await??;
        assert!(status.success(), "pattern {pattern:?} should allow startup");
        let logs = read_logs(dir.path())?;
        assert!(logs.contains("MCP_STDIO_STARTED"), "{logs}");
    }

    Ok(())
}

fn cli_allow_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
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
