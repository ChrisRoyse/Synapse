# Issue #1722 FSV - health/perception stalls - 2026-07-17

## Root cause

The live daemon could keep `127.0.0.1:7700` listening while `/health` and MCP response delivery hung. The physical SoT before the fix was:

- process/socket: PID `74140`, `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, listening on `127.0.0.1:7700`
- lifecycle row: `C:\Users\hotra\AppData\Local\synapse\db-daemon\daemon-run-current.json`, `ended_at_unix_ms=null`
- tool ledger: `C:\Users\hotra\AppData\Local\synapse\db-daemon\daemon-tool-last.json`, seq `115`, `tool=observe`, `status=started`, no completion row

First-principles invariant: health and lifecycle readbacks must not wait behind user perception work, and perception stages must either complete or fail closed with enough evidence to repair the failing stage.

The broken structure violated that invariant in two places:

1. `/health` and dashboard state used blocking lock reads for session/state/runtime subsystems, so an unrelated in-flight operation could delay liveness/readiness evidence.
2. `observe`/`find` built window perception while holding the shared M1 state guard, and CDP/OCR enrichment had no bounded stage timeout. A slow or stuck UIA/CDP/OCR stage could leave the daemon with an in-flight tool row and no usable health verdict.

## Research used

Research was done with Exa MCP and native web research before implementation. Sources used for the design:

- Tokio shared-state guidance: https://tokio.rs/tokio/tutorial/shared-state
- Tokio `spawn_blocking`: https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
- Tokio task guidance: https://docs.rs/tokio/latest/tokio/task/
- Kubernetes liveness/readiness/startup probe guidance: https://kubernetes.io/docs/tasks/configure-pod-container/configure-liveness-readiness-startup-probes/

Applied conclusions: async health/readiness paths should be bounded and fail fast; blocking OS/UIA/OCR work belongs outside async worker threads and outside shared state locks; every external/blocking stage needs a timeout that reports the exact failing stage rather than silently degrading or falling back.

## Fix

- `/health` and dashboard state now use non-blocking session/state/runtime lock reads and return structured subsystem errors instead of waiting behind in-flight work.
- `M1ObservationSnapshot` copies immutable M1 settings while the M1 guard is held, then releases the guard before UIA/window perception.
- `observe`, `find`, `read_text`, action-delta, replay, reality, and context paths use the snapshot helpers instead of holding the M1 guard through perception work.
- Blocking perception gather runs under `spawn_blocking` with a bounded timeout.
- CDP enrichment and browser OCR enrichment are bounded. They clone the input, commit only on success, and fail closed on timeout or join failure with `MCP_PERCEPTION_STAGE_TIMEOUT` / `MCP_PERCEPTION_STAGE_JOIN_ERROR` logging.

No fallback path was added. Timeout or join failure returns a typed MCP error with source-of-truth and remediation text.

## Sources Of Truth

- Process/socket: `Get-Process synapse-mcp`, `Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort <port>`
- Lifecycle files:
  - isolated: `C:\Users\hotra\AppData\Local\synapse-fsv-1722-bounded\db\daemon-run-current.json`
  - production: `C:\Users\hotra\AppData\Local\synapse\db-daemon\daemon-run-current.json`
- Tool ledger:
  - isolated: `...\synapse-fsv-1722-bounded\db\daemon-tool-events.jsonl`, `daemon-tool-last.json`
  - production: `...\synapse\db-daemon\daemon-tool-events.jsonl`, `daemon-tool-last.json`
- Observation storage readback: `storage operation=inspect` reading real Calyx `CF_OBSERVATIONS`
- Runtime logs:
  - isolated: `C:\Users\hotra\AppData\Local\synapse-fsv-1722-bounded\logs\stderr.log`
  - setup: `C:\Users\hotra\AppData\Local\synapse\logs\setup-build.log` and setup console readback
- Configured tool-surface snapshot: `C:\Users\hotra\AppData\Roaming\synapse\codex-tool-surface.json`

## Manual FSV - isolated repo-built daemon

Daemon:

- PID `54412`
- binary `C:\code\Synapse\target\debug\synapse-mcp.exe`
- bind `127.0.0.1:7795`
- DB `C:\Users\hotra\AppData\Local\synapse-fsv-1722-bounded\db`
- lifecycle before: `ended_at_unix_ms=null`

Happy path:

- `/health` before ledger: only Calyx vault lifecycle row existed.
- Trigger: authenticated `GET /health`.
- Response: `ok=true`, elapsed `932 ms`, `http.status=ok`, `active_sessions=0`.
- Separate log read: `MCP_HTTP_HEALTH_DONE ok=true duration_ms=849`.
- Separate socket/lifecycle read: PID `54412` still listening, lifecycle still active.

MCP session/tool-list:

- Trigger: `initialize`, `notifications/initialized`, `tools/list` through HTTP MCP.
- Session id: `3a7324d5-93cc-4832-adf2-cc1710782c77`.
- Result: `tools/list` returned `40` public tools; `health`, `observe`, and `find` present.
- Separate health read: `active_sessions=1`; transport counters showed `request_started_total=3`, `request_completed_total=3`, `request_in_flight=0`.

MCP `tools/call health`:

- Trigger: `tools/call health` in session `3a7324d5-93cc-4832-adf2-cc1710782c77`.
- Response: HTTP `200`, `isError=false`, `ok=true`, elapsed `1204 ms`.
- Separate ledger read: seq `2` `tool=health` `status=started`, then seq `2` `status=ok`, `duration_ms=895`, `mcp_session_id=3a7324d5-93cc-4832-adf2-cc1710782c77`.
- Separate health read after: `in_flight_count=0`.

Concurrent observe/health:

- Synthetic UI SoT: Notepad window `SYNAPSE_FSV_1722_SENTINEL.txt - Notepad`, HWND `67122`.
- Trigger: background `tools/call observe` with `{ window_hwnd: 67122, include: ["elements","diagnostics"], depth: 6, max_elements: 500 }`, then authenticated `/health` while observe was in flight.
- Health result during observe: `ok=true`, elapsed `930 ms`; daemon lifecycle detail reported `in_flight_count=1`.
- Observe result: HTTP `200`, `isError=false`, elapsed `842 ms`.
- Separate ledger read: seq `4` `tool=observe` `status=started`, then seq `4` `status=ok`, `duration_ms=597`.

Observation storage readback:

- Before trigger: `storage operation=inspect` read `CF_OBSERVATIONS=1`.
- Trigger: explicit-HWND observe against HWND `67122`.
- Response: `observed_hwnd=67122`, `observed_process=Notepad.exe`, `element_count=17`, `a11y_status=healthy`.
- After trigger: `storage operation=inspect` read `CF_OBSERVATIONS=2`, `cf_observation_sample_count=2`.
- Separate ledger read: seq `6` `tool=observe` `status=ok`, `duration_ms=549`.

Edge 1 - invalid auth:

- Before: last tool row seq `7`, valid health `ok=true`, `active_sessions=1`.
- Trigger: unauthenticated `GET /health`.
- Result: HTTP `401`, elapsed `72 ms`.
- After: valid health still `ok=true`; tool ledger did not advance for the rejected unauthenticated request; listener remained alive.

Edge 2 - structurally invalid target:

- Before: `CF_OBSERVATIONS=2`.
- Trigger: `tools/call observe` with `{ window_hwnd: 0, include: ["elements"] }`.
- Result: JSON-RPC error, `TOOL_PARAMS_INVALID`, accepted range `1..=u32::MAX`, `field=window_hwnd`, with remediation and source-of-truth text.
- Separate ledger read: seq `9` and seq `10` `tool=observe` `status=error`, durations `111 ms` and `119 ms`.
- After: `CF_OBSERVATIONS=2`, health `ok=true`, `in_flight_count=0`.

Edge 3 - excessive limits:

- Before: `CF_OBSERVATIONS=2`.
- Trigger: `tools/call observe` with `{ window_hwnd: 67122, include: ["elements","diagnostics"], depth: 999, max_elements: 999999 }`.
- Result: HTTP `200`, `isError=false`, elapsed `859 ms`; target remained HWND `67122`; `element_count=87`; `a11y_status=healthy`.
- After: `CF_OBSERVATIONS=3`, `cf_observation_sample_count=3`.
- Separate ledger read: seq `12` `tool=observe` `status=ok`, `duration_ms=581`.

Shutdown / host hygiene:

- Trigger: authenticated `POST /shutdown` on `127.0.0.1:7795`.
- Result: HTTP `202`, elapsed `77 ms`.
- After process/socket read: PID `54412` absent; no listener; only TIME_WAIT sockets.
- Lifecycle read: `ended_at_unix_ms=1784288050964`, `ended_reason=graceful`.
- Exit ledger: `daemon_exit`, `cause=graceful`, `in_flight_tool_events=[]`, vault closed with lock/pid sidecar cleared.
- Synthetic Notepad PIDs were exact-PID cleaned and absent afterward.

## Manual FSV - configured installed daemon

Setup:

- Old configured daemon PID `74140` was `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, bind `127.0.0.1:7700`, with `daemon-tool-last.json` stuck at seq `115` `tool=observe` `status=started`.
- Authenticated shutdown returned HTTP `202` in `67 ms`, closed the listener, but the old process remained alive with no lifecycle end row. The verified exact PID/path/command line was terminated to clear the configured host for installation.
- `scripts\synapse-setup.ps1 -SourceDir C:\code\Synapse` built and candidate-validated the fixed release binary. Candidate health passed and candidate graceful shutdown passed.
- A first handoff attempt failed on an auto-started old daemon PID `76148` timing out during graceful shutdown; the exact verified PID was absent/cleared and setup was rerun.
- Final setup installed `C:\Users\hotra\.cargo\bin\synapse-mcp.exe` with sha256 `84A0D54093ACC2DBFBED37FDC320EB805A0073F86B438AFF6DC5D13037A367F2`, started scheduled task `SynapseMcpDaemon`, and launched daemon PID `44556`.
- Setup then failed closed with `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` because the already-running Codex process started with an older `episode` schema snapshot. Handoff written: `C:\Users\hotra\AppData\Local\synapse\codex-restart-handoffs\codex-restart-handoff-73344-20260717T114909125Z.json`.

Configured daemon SoT:

- Process: PID `44556`, `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`
- Socket: `127.0.0.1:7700` LISTEN owned by PID `44556`
- Task: `SynapseMcpDaemon` state `Running`
- Lifecycle: `run_id=1784288916677-44556-019f6fe834c5768385658538f77d537c`, `ended_at_unix_ms=null`
- Token file, user env, and current process env all matched.

Configured HTTP FSV:

- `/health`: `ok=true`, PID `44556`, elapsed `564 ms`, `http.status=ok`, `active_sessions=0`, `in_flight_count=0`, Chrome bridge `ok`.
- `initialize` + `notifications/initialized` + `tools/list`: session `82aa27cc-7190-41db-9bca-ba702b65f8fa`, `40` tools, `health`/`observe`/`find` present.
- Configured `tools/call health`: HTTP `200`, elapsed `3176 ms`, `isError=false`, PID `44556`, `daemon_lifecycle.status=ok`.
- Separate ledger read: seq `3` `tool=health` `status=ok`, `duration_ms=1833`, session `82aa27cc-7190-41db-9bca-ba702b65f8fa`.

Client-parity FSV:

- Real wired tool discovery exposed `mcp__synapse.health` and `mcp__synapse.find` in the current Codex process despite the stale `episode` schema handoff.
- Trigger: `mcp__synapse.health { detail: "compact" }`.
- Result: `ok=true`, PID `44556`, `tool_count=40`, `tool_surface_sha256=7baef0742b0aacbb2a838a301a3ef25af3175468b15d831f6aa90b88dfd7b776`.
- Separate ledger read: seq `4` `tool=health` `status=ok`, `profile=break_glass`, `duration_ms=1640`, session `21316980-d77d-411e-9447-9df9417a21a2`.
- Synthetic UI SoT: Notepad window `SYNAPSE_FSV_1722_SENTINEL.txt - Notepad`, HWND `106040924`.
- Trigger: `mcp__synapse.find { window_hwnd: 106040924, query: "SYNAPSE_FSV_1722_HEALTH_SENTINEL_2468", scope: "both", limit: 5 }`.
- Result: 3 real Notepad UI elements returned, including the target window/tab/text entries.
- Separate ledger read: seq `5` `tool=find` `status=ok`, `duration_ms=1334`, session `21316980-d77d-411e-9447-9df9417a21a2`.
- Synthetic Notepad PIDs were exact-PID cleaned and absent afterward.

## Structural checks

These are not FSV, only structural gates:

- `cargo fmt --all --check` - passed
- `cargo check -p synapse-mcp` - passed
- `cargo clippy --workspace --all-targets` - passed with pre-existing warning-only findings in `synapse-storage`
- `cargo build -p synapse-mcp` - passed before release setup

## Remaining notes

- The installed daemon is intentionally left running as the configured long-lived daemon, PID `44556`.
- The current Codex process has a stale `episode` schema snapshot, and setup wrote the restart handoff above. That did not prevent real wired `mcp__synapse.health` and `mcp__synapse.find` calls from succeeding and being recorded by the daemon, but a fresh Codex process should read the handoff and recovery notes.
