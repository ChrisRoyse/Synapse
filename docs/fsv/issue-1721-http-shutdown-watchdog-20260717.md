# Issue #1721 - HTTP Shutdown Watchdog FSV - 2026-07-17

## Scope

Issue: `synapse-mcp restart leaves process alive without HTTP listener`.

Root invariant: after an HTTP `/shutdown` request is accepted, the daemon must not remain alive without the configured HTTP listener. It must either complete graceful shutdown and exit with the supervisor-visible restart code, or force a nonzero fatal exit with structured evidence before the listener-less deadline.

## Root Cause

The HTTP shutdown path stopped the listener and then awaited owner cleanup. Historical production lifecycle rows showed shutdown could retain background owners (`transcript_ingest`, `ambient_ingest`, and sometimes `escalation_worker`) and storage owners, leaving the process alive after the listener was gone. Because the listener was already closed, the configured MCP client could no longer call tools, but the process still appeared alive in the OS process table.

Two code-level gaps made the failure class possible:

- Periodic transcript/ambient ingest tasks ran synchronous scan/ingest work inside async tasks. `JoinHandle::abort` only cancels at await points, so the shutdown drain could not stop a running scan until that scan returned.
- The HTTP endpoint shutdown branch did not have a process-level deadline after listener teardown, and one select branch could classify shutdown as `http_endpoint` while still returning `ExitCode::SUCCESS`.

## Research

Primary references used:

- Tokio graceful shutdown: detect shutdown, notify tasks, then wait for tasks to finish. https://tokio.rs/tokio/topics/shutdown
- `tokio_util::task::TaskTracker`: commonly paired with `CancellationToken` to wait for task completion after cancellation. https://docs.rs/tokio-util/latest/tokio_util/task/task_tracker/struct.TaskTracker.html
- Tokio `spawn_blocking`: running blocking tasks cannot be aborted after they start; shutdown can wait indefinitely for started blocking work. https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
- Axum graceful shutdown surface: `with_graceful_shutdown` wraps the serve future, so post-listener cleanup still needs its own bounded owner/readback discipline. https://docs.rs/axum/latest/axum/serve/struct.WithGracefulShutdown.html

Design conclusion: graceful cancellation is necessary but not sufficient for a daemon that becomes unreachable after listener teardown. The process also needs a fail-closed watchdog for the listener-less drain phase, plus cooperative cancellation checks inside the long synchronous scans.

## Fix Summary

- Added `SYNAPSE_HTTP_SHUTDOWN_WATCHDOG_TIMEOUT_SECS` with a default of 45 seconds. Invalid values fail startup; `0` is rejected.
- Armed an OS-thread watchdog when the HTTP listener shutdown path begins. If graceful shutdown does not reach a terminal process decision before the deadline, it records a forced-exit lifecycle row when possible, emits a fatal stderr line, and exits `1`.
- Disarmed the watchdog only after lifecycle finalization and `MCP_HTTP_PROCESS_EXIT_DECISION`.
- Made all accepted HTTP endpoint shutdown paths return exit code `1`, including the server-task select branch that can win after shutdown cancellation.
- Added cooperative cancellation checks to transcript and ambient ingest loops so shutdown can stop between source lines/directories without advancing cursors past uncommitted data.
- Added escalation worker shutdown checks after tick/signal wakeups and before orphan-toast cleanup.

## Manual FSV Environment

Repo binary: `C:\code\Synapse\target\debug\synapse-mcp.exe`.

FSV root: `C:\Users\hotra\AppData\Local\synapse\fsv\issue1721-final-20260717T0535`.

All FSV daemons used isolated DB/log/shell/transcript/ambient roots and `SYNAPSE_CALYX_VAULT=false`. The production daemon on `127.0.0.1:7700` was not used as the trigger target.

Configured-client precondition before FSV:

- Real `mcp__synapse.health` succeeded through the strict Codex MCP client.
- Production PID: `74140`.
- Production bind: `127.0.0.1:7700`.
- Tool count: `40`.
- Tool surface SHA: `7baef0742b0aacbb2a838a301a3ef25af3175468b15d831f6aa90b88dfd7b776`.

## Happy Path - Authenticated Shutdown With Active MCP Session

Source of Truth:

- OS process row for PID `87220`.
- TCP socket rows for `127.0.0.1:7791`.
- Lifecycle files in `...\db-happy`.
- Structured daemon log in `...\logs-happy\synapse.log.2026-07-17`.

Before trigger:

```text
ProcessId=87220
ExecutablePath=C:\code\Synapse\target\debug\synapse-mcp.exe
CommandLine includes --bind 127.0.0.1:7791 --db ...\db-happy

Socket: 127.0.0.1:7791 Listen OwningProcess=87220

daemon-run-current.json:
pid=87220
bind_addr=127.0.0.1:7791
ended_at_unix_ms=null
ended_reason=null

GET /health:
ok=true
pid=87220
tool_count=241

MCP initialize + tools/list:
session_id=9a3cff05-cb29-4706-b61c-00174e938712
tools/list body contained "name":"health"
```

Trigger:

```text
POST /shutdown with valid bearer token and User-Agent issue1721-happy-fsv
response ok=true
pid=87220
active_sessions_before_shutdown=1
reason_code=DAEMON_RESTARTING
```

After readback:

```text
Process table: no PID 87220 row
Socket: no LISTEN row on 127.0.0.1:7791; only TIME_WAIT rows

daemon-run-current.json:
ended_at_unix_ms=1784284231374
ended_reason=graceful

daemon-exit.jsonl tail:
event_kind=daemon_exit
cause=graceful
detail.source=http_service_completed
```

Structured log evidence:

```text
MCP_HTTP_SHUTDOWN_WATCHDOG_ARMED source=http_endpoint timeout_ms=15000 pid=87220
MCP_HTTP_SHUTDOWN_SESSIONS_CLOSED sessions_before=1 close_attempted=1 close_succeeded=1 sessions_after=0
MCP_HTTP_BACKGROUND_TASKS_FINAL_READBACK background_tasks_quiescent=true
MCP_HTTP_STORAGE_SERVICE_OWNER_FINAL_READBACK owners_quiescent=true
MCP_DAEMON_LIFETIME_LOCKS_CLOSED
MCP_HTTP_PROCESS_EXIT_DECISION exit_code=1 restart_requested=true
MCP_HTTP_SHUTDOWN_WATCHDOG_DISARMED outcome=http_service_completed
```

The Windows process handle was obtained with `Get-Process`, which did not expose a numeric exit code after exit. The real Windows exit code was verified in edge case 2 with the original `Start-Process -PassThru` handle.

## Edge 1 - Invalid Watchdog Configuration

Input: `SYNAPSE_HTTP_SHUTDOWN_WATCHDOG_TIMEOUT_SECS=0` on port `7792`.

Before:

```text
No synapse-mcp process for 127.0.0.1:7792
No socket row for 127.0.0.1:7792
```

Trigger:

```text
Start daemon with SYNAPSE_HTTP_SHUTDOWN_WATCHDOG_TIMEOUT_SECS=0
```

After:

```text
Process handle: pid=84808 has_exited=true exit_code=1
Process table: no PID 84808 row
Socket: no row for 127.0.0.1:7792
Lifecycle files: none, because startup failed before lifecycle ledger configuration
stderr: SYNAPSE_HTTP_SHUTDOWN_WATCHDOG_TIMEOUT_SECS must be at least 1 second
```

Verdict: invalid config fails closed and does not silently use a fallback timeout.

## Edge 2 - Unauthorized Shutdown Request

Input: wrong bearer token against port `7793`.

Before action:

```text
ProcessId=66412
Socket: 127.0.0.1:7793 Listen OwningProcess=66412
daemon-run-current.json ended_at_unix_ms=null ended_reason=null
```

Trigger:

```text
POST /shutdown with Authorization: Bearer definitely-wrong-token
```

Rejected after-state:

```text
HTTP status=401
ProcessId=66412 still present
Socket: 127.0.0.1:7793 Listen OwningProcess=66412
daemon-run-current.json ended_at_unix_ms=null ended_reason=null
Log: HTTP_TOKEN_INVALID reason=Invalid path=/shutdown
```

Authorized cleanup trigger:

```text
POST /shutdown with valid bearer token
```

Cleanup after-state:

```text
Process handle: pid=66412 has_exited=true exit_code=1
Process table: no PID 66412 row
Socket: no LISTEN row on 127.0.0.1:7793
daemon-run-current.json ended_reason=graceful
daemon-exit.jsonl cause=graceful detail.source=http_service_completed
Log: MCP_HTTP_PROCESS_EXIT_DECISION exit_code=1 restart_requested=true
Log: MCP_HTTP_SHUTDOWN_WATCHDOG_DISARMED
```

Verdict: unauthorized shutdown cannot enter drain; authorized shutdown exits with the supervisor-visible restart code.

## Edge 3 - Bind Collision

Input: a synthetic `TcpListener` occupied `127.0.0.1:7794` before daemon start.

Before:

```text
No synapse-mcp process for 127.0.0.1:7794
No socket row for 127.0.0.1:7794
```

Blocker state:

```text
Socket: 127.0.0.1:7794 Listen OwningProcess=65576
```

Trigger:

```text
Start synapse-mcp with --bind 127.0.0.1:7794
```

After failure:

```text
Process handle: pid=79268 has_exited=true exit_code=1
Process table: no PID 79268 row
Socket while blocker active: 127.0.0.1:7794 Listen OwningProcess=65576
daemon-run-current.json ended_reason=top_level_error
daemon-exit.jsonl detail.error="bind HTTP MCP transport to 127.0.0.1:7794: Only one usage of each socket address ... (os error 10048)"
stderr contained the same bind error
```

Cleanup readback:

```text
Synthetic listener stopped
No socket row remained for 127.0.0.1:7794
No issue1721-final synapse-mcp processes remained
```

Verdict: bind failure is loud, recorded in the lifecycle ledger, and does not leave a daemon process behind.

## Structural Checks

Completed after the final edit:

```text
cargo fmt --all --check
cargo check --workspace
cargo clippy --workspace --all-targets
cargo build -p synapse-mcp
git diff --check
```

`cargo clippy --workspace --all-targets` exited 0. It still printed existing pedantic warnings in `synapse-storage` and `crates/synapse-storage/examples/dump_cf.rs`, but no warning remained in the #1721 `synapse-mcp` diff.

## Follow-Up Filed

During FSV, the production daemon showed a separate listener-alive but `/health`-hung state. Filed as #1722: `[BUG] synapse-mcp listener can stay alive while /health and MCP response delivery hang`.
