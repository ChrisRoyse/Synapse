//! `--mode doctor`: enumerate, classify, and optionally clean up synapse-mcp
//! processes. Operationalizes the manual triage used to recover from leaked /
//! duplicate instances: it names the legitimate daemon (the RocksDB lock
//! holder recorded by the single-instance guard), classifies every other
//! synapse-mcp process, and with `--kill-stray` removes everything except the
//! one legitimate daemon (and itself).

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, get_current_pid};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Daemon,
    Bridge,
    Doctor,
    StrayStdio,
    Orphan,
    Unknown,
}

/// Run the doctor report. With `kill_stray`, terminate every synapse-mcp
/// process that is neither the legitimate daemon nor this doctor process.
#[must_use]
pub fn run_doctor(kill_stray: bool, db: Option<&Path>) -> ExitCode {
    let self_pid = get_current_pid().ok().map(|p| p.as_u32());
    let db_path = db.map_or_else(crate::m3::default_db_path, Path::to_path_buf);
    let target_is_default = paths_equivalent(&db_path, &crate::m3::default_db_path());
    let lock_holder = crate::single_instance::SingleInstanceGuard::recorded_holder_pid(&db_path);

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
        if !process
            .name()
            .to_string_lossy()
            .to_lowercase()
            .contains("synapse-mcp")
        {
            continue;
        }
        let pid_u = pid.as_u32();
        let args = process
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let is_lock_holder = Some(pid_u) == lock_holder;
        if !process_targets_db(&args, &db_path, target_is_default, is_lock_holder) {
            ignored_other_db += 1;
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
        found.push((pid_u, kind, cmd));
    }
    found.sort_by_key(|(pid, _, _)| *pid);

    println!(
        "synapse-mcp doctor\n  db_path          = {}\n  lock_holder      = {}\n  processes        = {}\n  ignored_other_db = {}",
        db_path.display(),
        lock_holder.map_or_else(|| "<none>".to_owned(), |p| p.to_string()),
        found.len(),
        ignored_other_db,
    );
    for (pid, kind, cmd) in &found {
        let marker = if Some(*pid) == lock_holder {
            " <- lock holder"
        } else {
            ""
        };
        println!("  pid={pid:<8} kind={kind:?}{marker}\n      cmd: {cmd}");
    }
    tracing::info!(
        code = "MCP_DOCTOR_REPORT",
        db_path = %db_path.display(),
        lock_holder = lock_holder.map_or_else(|| "none".to_owned(), |p| p.to_string()),
        process_count = found.len(),
        ignored_other_db,
        daemon_count = found.iter().filter(|(_, k, _)| *k == Kind::Daemon).count(),
        stray_count = found
            .iter()
            .filter(|(_, k, _)| matches!(k, Kind::Bridge | Kind::StrayStdio | Kind::Orphan | Kind::Unknown))
            .count(),
        "doctor enumerated synapse-mcp processes"
    );

    if kill_stray {
        if lock_holder.is_none() || !found.iter().any(|(_, kind, _)| *kind == Kind::Daemon) {
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
            .filter(|(_, k, _)| {
                matches!(
                    k,
                    Kind::Bridge | Kind::StrayStdio | Kind::Orphan | Kind::Unknown
                )
            })
            .count();
        for (pid, kind, _) in &found {
            if *kind == Kind::Daemon || *kind == Kind::Doctor || Some(*pid) == self_pid {
                continue;
            }
            if let Some(process) = system.process(Pid::from_u32(*pid)) {
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

fn process_targets_db(
    args: &[String],
    target_db: &Path,
    target_is_default: bool,
    is_lock_holder: bool,
) -> bool {
    if is_lock_holder {
        return true;
    }
    match explicit_db_arg_matches(args, target_db) {
        Some(matches) => matches,
        None => target_is_default,
    }
}

fn explicit_db_arg_matches(args: &[String], target_db: &Path) -> Option<bool> {
    for (idx, arg) in args.iter().enumerate() {
        if arg == "--db" {
            return Some(
                args.get(idx + 1)
                    .is_some_and(|raw| paths_equivalent(Path::new(raw), target_db)),
            );
        }
        if let Some(raw) = arg.strip_prefix("--db=") {
            return Some(paths_equivalent(Path::new(raw), target_db));
        }
    }
    None
}

fn paths_equivalent(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

fn path_key(path: &Path) -> String {
    let path = path.canonicalize().unwrap_or_else(|_| PathBuf::from(path));
    let mut raw = path.to_string_lossy().replace('/', "\\");
    while raw.ends_with('\\') {
        raw.pop();
    }
    #[cfg(windows)]
    {
        raw.make_ascii_lowercase();
    }
    raw
}
