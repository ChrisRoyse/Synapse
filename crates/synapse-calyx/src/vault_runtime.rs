//! The names the Synapse daemon owns *inside* the vault directory.
//!
//! # Why this list exists here
//!
//! The vault directory is both the Calyx vault root and the daemon's runtime
//! root. The daemon writes its single-instance lock, its pid sidecar, its
//! lifecycle lock, and its lifecycle ledgers into that same directory. None of
//! those are vault data:
//!
//! * The lock files are held with `LockFileEx` byte-range locks. Microsoft's
//!   contract is explicit — "if the locking process opens the file a second
//!   time, it cannot access the specified region through this second handle
//!   until it unlocks the region", and "locking a region that goes beyond the
//!   current end-of-file position is not an error". A backup running *inside*
//!   the daemon therefore cannot read even a 0-byte lock token it holds itself;
//!   the read fails with `ERROR_LOCK_VIOLATION` (os error 33). That is why
//!   `storage operation=backup` had never once completed against a live daemon.
//! * The pid/lifecycle records describe the process that is running *now*, not
//!   the process that will one day open the restored copy. This is exactly why
//!   `PostgreSQL` omits `postmaster.pid` and `postmaster.opts` from a base backup:
//!   they "record information about the running postmaster, not about the
//!   postmaster which will eventually use this backup".
//!
//! The list is deliberately **explicit and named**. A blanket "skip files that
//! cannot be read" rule would let a genuinely unreadable SST drop silently out
//! of a backup, which is the silent degradation this codebase forbids. Anything
//! not named here that cannot be read still fails the backup closed.
//!
//! The constants live in this crate — not in `synapse-mcp`, which creates the
//! files — because the vault-directory contract (what is data and what is
//! runtime) belongs to the crate that owns the vault. `synapse-mcp` consumes
//! these constants so the two can never drift apart.

/// Daemon single-instance advisory lock token (`fs2` / `LockFileEx`).
pub const DAEMON_LOCK_FILE: &str = "daemon.lock";
/// Unlocked sidecar holding the current single-instance lock holder's pid.
pub const DAEMON_PID_FILE: &str = "daemon.pid";
/// Lock token serialising daemon lifecycle-record writers.
pub const DAEMON_LIFECYCLE_LOCK_FILE: &str = "daemon-lifecycle.lock";
/// Record of the run that is live right now (pid, bind address, run id).
pub const DAEMON_RUN_CURRENT_FILE: &str = "daemon-run-current.json";
/// Most recent tool invocation of the live run.
pub const DAEMON_TOOL_LAST_FILE: &str = "daemon-tool-last.json";
/// Active segment of the daemon tool-event ledger.
pub const DAEMON_TOOL_EVENTS_FILE: &str = "daemon-tool-events.jsonl";
/// Active segment of the daemon exit-event ledger.
pub const DAEMON_EXIT_EVENTS_FILE: &str = "daemon-exit.jsonl";

/// Number of rotated segments retained beside each active lifecycle ledger
/// (`<ledger>.1` … `<ledger>.5`, newest suffix `.1`).
pub const MAX_LIFECYCLE_LEDGER_SEGMENTS: usize = 5;

/// Every top-level vault-directory file name the daemon owns, including the
/// rotated lifecycle-ledger segments.
///
/// Returned as owned strings because the rotated segment names are derived from
/// [`MAX_LIFECYCLE_LEDGER_SEGMENTS`] rather than written out one by one.
#[must_use]
pub fn daemon_runtime_file_names() -> Vec<String> {
    let mut names = vec![
        DAEMON_LOCK_FILE.to_owned(),
        DAEMON_PID_FILE.to_owned(),
        DAEMON_LIFECYCLE_LOCK_FILE.to_owned(),
        DAEMON_RUN_CURRENT_FILE.to_owned(),
        DAEMON_TOOL_LAST_FILE.to_owned(),
        DAEMON_TOOL_EVENTS_FILE.to_owned(),
        DAEMON_EXIT_EVENTS_FILE.to_owned(),
    ];
    for ledger in [DAEMON_TOOL_EVENTS_FILE, DAEMON_EXIT_EVENTS_FILE] {
        for index in 1..=MAX_LIFECYCLE_LEDGER_SEGMENTS {
            names.push(format!("{ledger}.{index}"));
        }
    }
    names
}
