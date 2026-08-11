//! `--mode doctor`: enumerate, classify, and optionally clean up synapse-mcp
//! processes. Operationalizes the manual triage used to recover from leaked /
//! duplicate instances: it names the legitimate daemon (the storage lock
//! holder recorded by the single-instance guard), classifies every other
//! synapse-mcp process, and with `--kill-stray` removes everything except the
//! one legitimate daemon (and itself).

use std::{fs::File, io::Read as _, path::Path, process::ExitCode};

use sha2::{Digest as _, Sha256};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, get_current_pid};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Daemon,
    Bridge,
    Doctor,
    StrayStdio,
    Orphan,
    Unknown,
    UnverifiedCandidate,
}

/// Run the doctor report. With `kill_stray`, terminate every byte-verified
/// Synapse process that is neither the legitimate daemon nor this doctor.
/// CLI-shaped candidates from different builds remain visible but are never
/// killed because their executable identity is not owned by this build.
#[must_use]
pub fn run_doctor(kill_stray: bool, db: Option<&Path>) -> ExitCode {
    let self_pid = get_current_pid().ok().map(|p| p.as_u32());
    let db_path = db.map_or_else(crate::m3::default_db_path, Path::to_path_buf);
    let target_is_default = match paths_equivalent(&db_path, &crate::m3::default_db_path()) {
        Ok(matches) => matches,
        Err(error) => {
            eprintln!(
                "synapse-mcp doctor: MCP_DOCTOR_DB_IDENTITY_FAILED db={} default_db={} error={error}",
                db_path.display(),
                crate::m3::default_db_path().display()
            );
            return ExitCode::from(2);
        }
    };
    let lock_holder = crate::single_instance::SingleInstanceGuard::recorded_holder_pid(&db_path);
    let current_exe = match std::env::current_exe().and_then(std::fs::canonicalize) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("synapse-mcp doctor: MCP_DOCTOR_CURRENT_EXE_READ_FAILED error={error}");
            return ExitCode::from(2);
        }
    };
    let current_exe_sha256 = match sha256_file(&current_exe) {
        Ok(hash) => hash,
        Err(error) => {
            eprintln!(
                "synapse-mcp doctor: MCP_DOCTOR_CURRENT_EXE_HASH_FAILED path={} error={error}",
                current_exe.display()
            );
            return ExitCode::from(2);
        }
    };

    let mut system = System::new();
    // `everything()` is required to populate each process's command line; the
    // default refresh leaves cmd() empty, which would collapse all
    // classification to the stray fallback.
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );

    let mut found = Vec::new();
    let mut ignored_other_db = 0_usize;
    for (pid, process) in system.processes() {
        let named_synapse = process
            .name()
            .to_string_lossy()
            .to_lowercase()
            .contains("synapse-mcp");
        let pid_u = pid.as_u32();
        let args = process
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let is_lock_holder = Some(pid_u) == lock_holder;
        let targets_db = match process_targets_db(
            &args,
            &db_path,
            target_is_default,
            is_lock_holder,
        ) {
            Ok(targets_db) => targets_db,
            Err(error) => {
                eprintln!(
                    "synapse-mcp doctor: MCP_DOCTOR_CANDIDATE_DB_IDENTITY_FAILED pid={pid_u} db={} error={error}",
                    db_path.display()
                );
                return ExitCode::from(2);
            }
        };
        if !targets_db {
            if named_synapse || synapse_cli_shape(&args) {
                ignored_other_db += 1;
            }
            continue;
        }
        if !named_synapse && !synapse_cli_shape(&args) && Some(pid_u) != self_pid && !is_lock_holder
        {
            continue;
        }
        let Some(exe_path) = process.exe() else {
            continue;
        };
        let exe_sha256 = match sha256_file(exe_path) {
            Ok(hash) => hash,
            Err(error) => {
                tracing::warn!(
                    code = "MCP_DOCTOR_CANDIDATE_EXE_HASH_FAILED",
                    pid = pid_u,
                    exe_path = %exe_path.display(),
                    error = %error,
                    "candidate was not admitted as a Synapse process because its executable bytes could not be verified"
                );
                continue;
            }
        };
        if exe_sha256 != current_exe_sha256 && !is_lock_holder {
            tracing::warn!(
                code = "MCP_DOCTOR_CANDIDATE_BUILD_MISMATCH",
                pid = pid_u,
                exe_path = %exe_path.display(),
                candidate_sha256 = %exe_sha256,
                doctor_sha256 = %current_exe_sha256,
                "candidate CLI targeted the selected DB but its executable bytes do not match this doctor build; refusing destructive classification"
            );
            found.push((
                pid_u,
                Kind::UnverifiedCandidate,
                args.join(" "),
                process.start_time(),
                exe_path.to_path_buf(),
                exe_sha256,
            ));
            continue;
        }
        let cmd = args.join(" ");
        let parent_alive = process
            .parent()
            .is_some_and(|pp| system.process(pp).is_some());
        let kind = if is_lock_holder {
            Kind::Daemon
        } else if cmd.contains("--mode connect") {
            Kind::Bridge
        } else if Some(pid_u) == self_pid || cmd.contains("--mode doctor") {
            Kind::Doctor
        } else if !parent_alive {
            Kind::Orphan
        } else if cmd.contains("--mode stdio") || !cmd.contains("--mode") {
            Kind::StrayStdio
        } else {
            Kind::Unknown
        };
        found.push((
            pid_u,
            kind,
            cmd,
            process.start_time(),
            exe_path.to_path_buf(),
            exe_sha256,
        ));
    }
    found.sort_by_key(|(pid, ..)| *pid);

    println!(
        "synapse-mcp doctor\n  db_path          = {}\n  lock_holder      = {}\n  processes        = {}\n  ignored_other_db = {}",
        db_path.display(),
        lock_holder.map_or_else(|| "<none>".to_owned(), |p| p.to_string()),
        found.len(),
        ignored_other_db,
    );
    for (pid, kind, cmd, start_time, exe_path, exe_sha256) in &found {
        let marker = if Some(*pid) == lock_holder {
            " <- lock holder"
        } else {
            ""
        };
        println!(
            "  pid={pid:<8} kind={kind:?}{marker}\n      start_time: {start_time}\n      exe: {}\n      sha256: {exe_sha256}\n      cmd: {cmd}",
            exe_path.display()
        );
    }
    tracing::info!(
        code = "MCP_DOCTOR_REPORT",
        db_path = %db_path.display(),
        lock_holder = lock_holder.map_or_else(|| "none".to_owned(), |p| p.to_string()),
        process_count = found.len(),
        ignored_other_db,
        daemon_count = found.iter().filter(|(_, k, ..)| *k == Kind::Daemon).count(),
        stray_count = found
            .iter()
            .filter(|(_, k, ..)| matches!(k, Kind::Bridge | Kind::StrayStdio | Kind::Orphan | Kind::Unknown))
            .count(),
        unverified_candidate_count = found
            .iter()
            .filter(|(_, k, ..)| *k == Kind::UnverifiedCandidate)
            .count(),
        "doctor enumerated synapse-mcp processes"
    );

    if kill_stray {
        if lock_holder.is_none() || !found.iter().any(|(_, kind, ..)| *kind == Kind::Daemon) {
            let remediation = "start the shared daemon for this --db path, or rerun doctor with the daemon's --db";
            println!("  refusing cleanup: no live lock-holder daemon found; {remediation}");
            tracing::error!(
                code = "MCP_DOCTOR_NO_LIVE_DAEMON",
                db_path = %db_path.display(),
                remediation,
                "refusing --kill-stray without a live lock-holder daemon"
            );
            return ExitCode::from(2);
        }

        let mut killed = 0_usize;
        let mut failed = 0_usize;
        let cleanup_targets = found
            .iter()
            .filter(|(_, k, ..)| {
                matches!(
                    k,
                    Kind::Bridge | Kind::StrayStdio | Kind::Orphan | Kind::Unknown
                )
            })
            .count();
        for (pid, kind, _, expected_start_time, expected_exe, expected_sha256) in &found {
            if *kind == Kind::Daemon || *kind == Kind::Doctor || Some(*pid) == self_pid {
                continue;
            }
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[Pid::from_u32(*pid)]),
                true,
                ProcessRefreshKind::everything(),
            );
            if let Some(process) = system.process(Pid::from_u32(*pid)) {
                let current_exe = process.exe().map(Path::to_path_buf);
                let current_sha256 = current_exe.as_deref().map(sha256_file).transpose();
                let current_path_matches = current_exe
                    .as_deref()
                    .map(|path| paths_equivalent(path, expected_exe))
                    .transpose();
                let identity_matches = process.start_time() == *expected_start_time
                    && current_path_matches
                        .as_ref()
                        .is_ok_and(|value| *value == Some(true))
                    && current_sha256
                        .as_ref()
                        .is_ok_and(|hash| hash.as_ref() == Some(expected_sha256));
                if !identity_matches {
                    failed += 1;
                    println!(
                        "  refused pid={pid} ({kind:?}): exact process identity changed before kill"
                    );
                    tracing::error!(
                        code = "MCP_DOCTOR_KILL_IDENTITY_DRIFT",
                        pid = *pid,
                        kind = ?kind,
                        expected_start_time = *expected_start_time,
                        actual_start_time = process.start_time(),
                        expected_exe = %expected_exe.display(),
                        actual_exe = ?current_exe,
                        current_path_matches = ?current_path_matches,
                        expected_sha256 = %expected_sha256,
                        actual_sha256 = ?current_sha256,
                        "refusing destructive cleanup because the exact PID generation or executable bytes changed"
                    );
                    continue;
                }
                if process.kill() {
                    killed += 1;
                    println!("  killed pid={pid} ({kind:?})");
                    tracing::warn!(
                        code = "MCP_DOCTOR_KILLED_STRAY",
                        pid = *pid,
                        kind = ?kind,
                        "killed stray synapse-mcp process"
                    );
                } else {
                    failed += 1;
                    let remediation =
                        "rerun from an elevated shell or terminate the named process manually";
                    println!("  failed to kill pid={pid} ({kind:?}); {remediation}");
                    tracing::error!(
                        code = "MCP_DOCTOR_KILL_FAILED",
                        pid = *pid,
                        kind = ?kind,
                        remediation,
                        "failed to kill stray synapse-mcp process"
                    );
                }
            }
        }
        println!("  killed {killed}/{cleanup_targets} cleanup target process(es)");
        if failed > 0 {
            return ExitCode::from(3);
        }
    }

    ExitCode::SUCCESS
}

fn synapse_cli_shape(args: &[String]) -> bool {
    let mode = args.iter().enumerate().find_map(|(index, arg)| {
        if arg == "--mode" {
            args.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--mode=")
        }
    });
    mode.is_some_and(|mode| {
        matches!(
            mode,
            "stdio"
                | "http"
                | "connect"
                | "chrome-native-host"
                | "approval-protocol"
                | "desktop-worker"
                | "detection-worker"
                | "doctor"
                | "local-agent"
        )
    })
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    Ok(output)
}

fn process_targets_db(
    args: &[String],
    target_db: &Path,
    target_is_default: bool,
    is_lock_holder: bool,
) -> std::io::Result<bool> {
    if is_lock_holder {
        return Ok(true);
    }
    Ok(explicit_db_arg_matches(args, target_db)?.unwrap_or(target_is_default))
}

fn explicit_db_arg_matches(args: &[String], target_db: &Path) -> std::io::Result<Option<bool>> {
    for (idx, arg) in args.iter().enumerate() {
        if arg == "--db" {
            return args
                .get(idx + 1)
                .map(|raw| paths_equivalent(Path::new(raw), target_db))
                .transpose();
        }
        if let Some(raw) = arg.strip_prefix("--db=") {
            return paths_equivalent(Path::new(raw), target_db).map(Some);
        }
    }
    Ok(None)
}

fn paths_equivalent(left: &Path, right: &Path) -> std::io::Result<bool> {
    Ok(path_key(left)? == path_key(right)?)
}

fn path_key(path: &Path) -> std::io::Result<String> {
    let path = path.canonicalize()?;
    let mut raw = path.to_string_lossy().replace('/', "\\");
    while raw.ends_with('\\') {
        raw.pop();
    }
    #[cfg(windows)]
    {
        raw.make_ascii_lowercase();
    }
    Ok(raw)
}
