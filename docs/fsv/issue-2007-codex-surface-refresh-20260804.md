# Issue #2007: Codex surface diagnostic refresh FSV (2026-08-04)

## Diagnosis and research

Setup writes `%APPDATA%\synapse\codex-tool-surface.json` only after the
installed daemon starts. `ImmutableToolSurface` cached a complete profile
snapshot during daemon construction, including that mutable file/process/
handoff diagnostic, so profile and telemetry cloned the previous installation
for the daemon's lifetime.

The fix retains the measured 20-22 ms memoization of sanitized schemas,
registry validation, and facade contracts, while refreshing only
`codex_client_surface` on an explicit profile/telemetry snapshot. The Exa lane
was live. Built-in research used Rust `std::fs::read_to_string`/`metadata`
documentation and Microsoft file-change documentation; an on-demand read is
the smaller fail-loud design for this infrequent correctness diagnostic.

## Installed happy path

Source of truth: the snapshot bytes, live daemon health fingerprint, current
handoff JSON, and OS process table. Setup installed `a31776ec`, started PID
19652, wrote the snapshot, wrote a current #2007 handoff, and then correctly
returned `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` because this Codex
process began with materially different schemas.

Without restarting that daemon, public `profile operation=status` read:

```text
host snapshot bytes=1978001
file sha256=57a8282edccb54abb003d8276bdcacde1b9520b0d29438bb18780b18ee6e8bcd
host/live surface=72251f9efe1467147a08e89e0f7aa2b2c03af256f9b7c03cdaf446d6bd3ece0b
host_snapshot_matches_live_tool_surface=true
handoff active_issue_ref=#2007 daemon_pid=19652 live_daemon_pid=19652
daemon_pid_matches_live_daemon=true
```

## Boundary audit

The original snapshot was copied byte-for-byte, and its SHA-256 was checked
before each controlled mutation and after restoration.

```text
MISSING before: exists=false backup_exists=true
MISSING after: status=host_snapshot_missing
               code=CODEX_CLIENT_SURFACE_HOST_SNAPSHOT_MISSING

CORRUPT before: bytes=12 sha256=d7aeddb6... content={broken-json
CORRUPT after: status=host_snapshot_read_error
               code=CODEX_CLIENT_SURFACE_HOST_SNAPSHOT_READ_ERROR
               read_error="key must be a string at line 1 column 2"

MISMATCH before: tool_count=40 stored_surface=0000...0000
MISMATCH after: matches=false
                detail names stored zero hash and exact live 72251f...ece0b

RESTORED after: bytes=1978001 sha256=57a8282e...e8bcd
                backup_exists=false daemon_pid=19652
                host_snapshot_matches_live_tool_surface=true
```

The restored SHA-256 exactly equals the initial SHA-256. The authoritative
`scripts/lint.ps1 -Fix` gate passed all seven checks in both workspaces before
deployment.
