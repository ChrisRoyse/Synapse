//! Manual FSV for exact process parentage capture (#2089).
//!
//! # What was wrong, and therefore what has to be proven
//!
//! Every `CF_PROCESS_HISTORY` row on the live vault (1085 of them) recorded the
//! pid of the process that was launched and nothing whatsoever about who
//! launched it. The `syn-graphpos-process-v1` process lane reads
//! `parent_pid | ppid | inherited_from_pid`; none of those keys existed on any
//! row, so the lane derived **zero** parent/child edges while the panel kept
//! publishing on its agent-spawn lane alone.
//!
//! Recording a parent pid is not enough on its own. Windows frees a pid for
//! reuse as soon as the process object dies, so a recorded parent pid can end up
//! naming an unrelated, later process — the classic caveat in
//! `devblogs.microsoft.com/oldnewthing/20150403-00`. The mitigation this module
//! implements, and that this instrument proves, is the one both that post and
//! `trainsec.net/library/windows-internals/building-a-process-tree/` land on:
//! carry the **creation time** of the child and of the claimed parent, and
//! refuse the edge unless the parent was created at or before the child.
//!
//! # Where the truth is read from
//!
//! | Part | Claim | Source of truth |
//! |------|-------|-----------------|
//! | 1 | a real, known 4-deep process chain is recovered exactly | pids this harness itself spawned and read back, matched against the kernel snapshot |
//! | 2 | every recovered link carries the reuse guard | `parent_start_time_100ns <= child_start_time_100ns` on each captured record |
//! | 3 | a parent that has exited yields no edge, with a reason | the orphaned child, captured after its parent is killed |
//! | 4 | a root process is recorded as a root, not as missing data | pid 4 (`System`), whose kernel parent is 0 |
//! | 5 | a protected process does not degrade into a silent `None` | `OpenProcess` denied on the same pid the snapshot reads fine |
//! | 6 | a recycled-pid record cannot produce an edge | a persisted record whose parent is younger than its child |
//!
//! Part 5 is the reason the observer uses
//! `NtQuerySystemInformation(SystemProcessInformation)` rather than
//! `OpenProcess` + `GetProcessTimes` per pid: the naive method fails
//! access-denied exactly where a silent null would be least distinguishable
//! from "no parent".
//!
//! ```text
//! cargo run -p synapse-action --example process_parentage_fsv
//! ```
//!
//! Spawns real processes (`pwsh` -> `cmd` -> `PING.EXE`) and kills them again.
//! Spawns nothing else and writes nothing.

use std::error::Error;
use std::process::{Command, Stdio};

use synapse_action::process_parentage::{
    ParentageState, ProcessParentage, capture_process_parentage,
};

struct Failures(Vec<String>);

impl Failures {
    fn check(&mut self, label: &str, held: bool, evidence: &str) {
        let verdict = if held { "PASS" } else { "FAIL" };
        println!("  [{verdict}] {label}: {evidence}");
        if !held {
            self.0.push(format!("{label}: {evidence}"));
        }
    }
}

fn print_record(tag: &str, record: &ProcessParentage) -> Result<(), Box<dyn Error>> {
    println!("  {tag} {}", serde_json::to_string(record)?);
    Ok(())
}

/// Spawn `pwsh -> cmd -> ping` and return `(pwsh_pid, cmd_pid)`.
///
/// The chain is built by the spawned shell itself, so the pids this harness
/// checks are the pids Windows really linked, not pids this harness assigned.
fn spawn_known_chain() -> Result<(std::process::Child, u32, u32), Box<dyn Error>> {
    // `Start-Process -PassThru` makes pwsh the creator of cmd, and `cmd /c ping`
    // makes cmd the creator of PING.EXE. The script prints both pids and then
    // blocks, so the whole chain is alive while it is observed.
    let script = "$c = Start-Process -FilePath cmd.exe -ArgumentList '/c','ping -n 120 \
                  127.0.0.1' -PassThru -WindowStyle Hidden; Write-Output \
                  \"CHAIN $PID $($c.Id)\"; Start-Sleep -Seconds 120";
    let mut child = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or("spawned pwsh exposed no stdout pipe")?;
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let read = std::io::BufRead::read_line(&mut reader, &mut line)?;
        if read == 0 {
            return Err("spawned pwsh closed stdout before announcing its chain".into());
        }
        let Some(rest) = line.trim().strip_prefix("CHAIN ") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let pwsh_pid: u32 = parts.next().ok_or("chain line had no pwsh pid")?.parse()?;
        let cmd_pid: u32 = parts.next().ok_or("chain line had no cmd pid")?.parse()?;
        return Ok((child, pwsh_pid, cmd_pid));
    }
}

/// The pid of the single child of `parent_pid`, read from the kernel snapshot.
#[cfg(windows)]
fn only_child_of(parent_pid: u32) -> Option<(u32, String)> {
    let snapshot = synapse_action::process_parentage::system_process_snapshot().ok()?;
    let mut children = snapshot
        .values()
        .filter(|entry| entry.parent_pid == parent_pid)
        .map(|entry| {
            (
                entry.pid,
                entry.image_name.clone().unwrap_or_else(|| "?".to_owned()),
            )
        })
        .collect::<Vec<_>>();
    children.sort_unstable();
    children.into_iter().next()
}

#[cfg(not(windows))]
fn only_child_of(_parent_pid: u32) -> Option<(u32, String)> {
    None
}

/// Try to read a protected process's creation time the naive way.
#[cfg(windows)]
fn open_process_denied(pid: u32) -> Result<(), String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: a plain query-rights open of a pid; the returned handle is closed
    // immediately below and never escapes.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|error| error.to_string())?;
    // SAFETY: `handle` came from the successful open directly above.
    let _ = unsafe { CloseHandle(handle) };
    Ok(())
}

#[cfg(not(windows))]
fn open_process_denied(_pid: u32) -> Result<(), String> {
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one FSV run: spawn, observe, kill and re-observe are a single ordered narrative"
)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut f = Failures(Vec::new());
    let self_pid = std::process::id();
    println!("process_parentage_fsv  (#2089)\nobserver_pid = {self_pid}");

    // ------------------------------------------------------------------
    // Parts 1 & 2 — a real chain, recovered exactly, with the guard on it
    // ------------------------------------------------------------------
    println!("\n== Parts 1-2: a known pwsh -> cmd -> ping chain ==");
    let (mut pwsh, pwsh_pid, cmd_pid) = spawn_known_chain()?;
    println!("  SPAWNED observer={self_pid} pwsh={pwsh_pid} cmd={cmd_pid}");
    // `cmd /c ping` execs PING.EXE as its own child; give it a moment to exist.
    let mut ping = None;
    for _ in 0..50 {
        if let Some(found) = only_child_of(cmd_pid) {
            ping = Some(found);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let (ping_pid, ping_name) = ping.ok_or("cmd never spawned an observable ping child")?;
    println!("  SPAWNED ping={ping_pid} image={ping_name}");

    let links = [
        ("observer->pwsh", self_pid, pwsh_pid),
        ("pwsh->cmd", pwsh_pid, cmd_pid),
        ("cmd->ping", cmd_pid, ping_pid),
    ];
    for (label, expected_parent, child_pid) in links {
        let record = capture_process_parentage(child_pid);
        print_record(&format!("CAPTURED {label}"), &record)?;
        f.check(
            &format!("{label} state is parent_verified"),
            record.state == ParentageState::ParentVerified,
            &format!("state={}", record.state.as_str()),
        );
        f.check(
            &format!("{label} names the exact spawning pid"),
            record.edge_parent_pid() == Some(expected_parent),
            &format!(
                "edge_parent_pid={:?} expected={expected_parent} parent_image={:?}",
                record.edge_parent_pid(),
                record.parent_image_name
            ),
        );
        let guard_held = match (
            record.parent_start_time_100ns,
            record.child_start_time_100ns,
        ) {
            (Some(parent), Some(child)) => parent <= child,
            _ => false,
        };
        f.check(
            &format!("{label} carries the pid-reuse guard"),
            guard_held,
            &format!(
                "parent_start_time_100ns={:?} child_start_time_100ns={:?} source={}",
                record.parent_start_time_100ns,
                record.child_start_time_100ns,
                record.start_time_source
            ),
        );
    }
    let observer_link = capture_process_parentage(pwsh_pid);
    f.check(
        "the observer is recognised as its own child's parent",
        observer_link.parent_is_observer,
        &format!("parent_is_observer={}", observer_link.parent_is_observer),
    );

    // ------------------------------------------------------------------
    // Part 3 — kill the parent; the orphan must refuse the edge, with a reason
    // ------------------------------------------------------------------
    println!("\n== Part 3: parent exits, orphan must not fabricate an edge ==");
    // Kill only pwsh. `cmd` keeps running with a dangling parent pid, which is
    // exactly the state in which a naive reader would emit a false edge the
    // moment that pid is reused.
    pwsh.kill()?;
    let _ = pwsh.wait()?;
    let mut orphan = capture_process_parentage(cmd_pid);
    for _ in 0..50 {
        if orphan.state != ParentageState::ParentVerified {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        orphan = capture_process_parentage(cmd_pid);
    }
    print_record("CAPTURED orphan(cmd)", &orphan)?;
    f.check(
        "the orphan's parentage is unavailable, not verified",
        orphan.state == ParentageState::ParentIdentityUnavailable,
        &format!("state={}", orphan.state.as_str()),
    );
    f.check(
        "the orphan still records the pid the kernel named",
        orphan.parent_pid_observed == Some(pwsh_pid),
        &format!("parent_pid_observed={:?}", orphan.parent_pid_observed),
    );
    f.check(
        "the orphan yields no edge",
        orphan.edge_parent_pid().is_none(),
        &format!("edge_parent_pid={:?}", orphan.edge_parent_pid()),
    );
    f.check(
        "the orphan carries an exact reason",
        orphan
            .unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("recycled")),
        orphan.unavailable_reason.as_deref().unwrap_or("<none>"),
    );

    // Leave nothing running.
    let _ = Command::new("taskkill")
        .args(["/PID", &cmd_pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    // ------------------------------------------------------------------
    // Part 4 — a root process is a root, not an absence
    // ------------------------------------------------------------------
    println!("\n== Part 4: a root process (pid 4, System) ==");
    let system = capture_process_parentage(4);
    print_record("CAPTURED system(pid 4)", &system)?;
    f.check(
        "the kernel's rootmost process is recorded as a root with a reason",
        system.state == ParentageState::NoParentRecorded
            && system.unavailable_reason.is_some()
            && system.edge_parent_pid().is_none(),
        &format!(
            "state={} reason={:?}",
            system.state.as_str(),
            system.unavailable_reason
        ),
    );

    // ------------------------------------------------------------------
    // Part 5 — a protected process the naive method cannot read
    // ------------------------------------------------------------------
    println!("\n== Part 5: a protected process ==");
    let protected = capture_process_parentage(4);
    let open_result = open_process_denied(4);
    println!("  OPEN_PROCESS pid=4 result={open_result:?}");
    f.check(
        "the snapshot observer produces a decided record for a pid OpenProcess cannot read",
        protected.state != ParentageState::SnapshotUnavailable
            && protected.child_start_time_100ns.is_some(),
        &format!(
            "state={} child_start_time_100ns={:?} open_process_denied={}",
            protected.state.as_str(),
            protected.child_start_time_100ns,
            open_result.is_err()
        ),
    );

    // ------------------------------------------------------------------
    // Part 6 — a recycled pid, as it is actually persisted, yields no edge
    // ------------------------------------------------------------------
    println!("\n== Part 6: a recycled-pid record read back from its persisted form ==");
    // Written as JSON on purpose: this is the exact shape the graph reader
    // consumes out of CF_PROCESS_HISTORY, so the refusal is proven on the
    // persisted contract and not only on an in-memory value.
    let recycled: ProcessParentage = serde_json::from_str(
        r#"{
            "schema_version": 1,
            "state": "parent_pid_recycled",
            "child_pid": 4242,
            "child_start_time_100ns": 133000000000000000,
            "parent_pid_observed": 909,
            "parent_start_time_100ns": 133900000000000000,
            "parent_image_name": "unrelated.exe",
            "parent_is_observer": false,
            "observer_pid": 1,
            "start_time_source": "windows_ntquerysysteminformation_createtime_filetime_100ns",
            "source": "windows_ntquerysysteminformation_system_process_information",
            "observed_at_unix_ms": 1786000000000,
            "unavailable_reason": "pid 909 was created after pid 4242"
        }"#,
    )?;
    f.check(
        "a recycled-pid record yields no edge",
        recycled.edge_parent_pid().is_none(),
        &format!("edge_parent_pid={:?}", recycled.edge_parent_pid()),
    );
    // And the guard is enforced by the reader, not only by the writer's verdict:
    // a record that *claims* verification while its own times contradict it is
    // still refused.
    let mut lying = recycled;
    lying.state = ParentageState::ParentVerified;
    f.check(
        "a record claiming verification against its own timestamps is still refused",
        lying.edge_parent_pid().is_none(),
        &format!(
            "state={} parent_start={:?} child_start={:?} edge_parent_pid={:?}",
            lying.state.as_str(),
            lying.parent_start_time_100ns,
            lying.child_start_time_100ns,
            lying.edge_parent_pid()
        ),
    );
    // A legacy row that predates the guard decodes, and decodes as unusable.
    let legacy: ProcessParentage = serde_json::from_str(
        r#"{
            "schema_version": 1,
            "state": "platform_unsupported",
            "child_pid": 7,
            "parent_is_observer": false,
            "observer_pid": 1,
            "start_time_source": "unavailable_on_this_platform",
            "source": "unavailable_on_this_platform",
            "observed_at_unix_ms": 1786000000000
        }"#,
    )?;
    f.check(
        "a record with no optional fields at all still decodes and yields no edge",
        legacy.edge_parent_pid().is_none(),
        &format!("state={}", legacy.state.as_str()),
    );

    println!("\n== VERDICT ==");
    if f.0.is_empty() {
        println!("  ALL CHECKS PASSED");
        Ok(())
    } else {
        for failure in &f.0 {
            println!("  FAILED {failure}");
        }
        Err(format!("{} check(s) failed", f.0.len()).into())
    }
}
