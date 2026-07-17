# Issue #1733 FSV - installed-health timeout during slow Calyx startup

Date: 2026-07-17
Agent: Codex
Issue: https://github.com/ChrisRoyse/Synapse/issues/1733

## Result

Accepted for the installed-health readiness scope. `scripts/synapse-setup.ps1`
now uses an explicit 600-second installed daemon health deadline, exposes
`-InstallHealthTimeoutSeconds` for bounded operator overrides, and prints
periodic process/socket/startup-log readbacks while waiting. The daemon now logs
`MCP_DAEMON_STORAGE_AND_CALYX_OPEN_START` before the eager storage/Calyx startup
open, so setup failures can distinguish "still progressing before bind" from
"stuck with no startup evidence".

No workaround/fallback was added. Setup still fails closed after the deadline
and rolls back with process, socket, hash, and startup-log diagnostics. No
automated tests, FSV harnesses, benchmarks, or CI were created or run.

## Root Cause

The #1731 happy-path setup run fixed the release compiler environment, built a
real candidate, preflighted it on an isolated database, installed it, and
started the installed daemon. The live installed daemon then spent longer than
the hard-coded 180-second setup deadline opening the real Calyx-backed storage
path before binding HTTP.

Observed failure before the #1733 fix:

```text
setup_log=%LOCALAPPDATA%\synapse\logs\issue-1731-happy-setup.stdout.log
before_installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
candidate_sha256=0E4424E3CABD4AFCDC0E8CB71E71C7802340E91AC77244C7682DFE9585B09C79
preflight_pid=81640 tool_count=40
installed_candidate_pid=77800
installed_candidate_launch_utc=2026-07-17T19:01:29.5403374Z
single_instance_lock_acquired_utc=2026-07-17T19:01:29.818504Z
setup_health_timeout=180s
setup_action=rollback
```

Separate daemon log readback showed the candidate was not dead. It was still in
the real live Calyx/storage open and bound shortly after setup had already
rolled it back:

```text
storage_open_completed_utc=2026-07-17T19:05:56.944696Z
http_bound_utc=2026-07-17T19:05:57.554595Z
startup_time_before_bind=about 268s
```

The root cause was the setup script's timeout and diagnostics, not the installed
daemon health endpoint. A fixed 180-second deadline was too short for the
current live database, and setup did not print enough startup-log state to prove
whether the daemon was progressing.

## Best-Practice Research Inputs

The same release-build research from #1731 explains why the first setup pass got
far enough to expose this issue. Deployment-health guidance also shaped this
fix:

- Microsoft Windows Installer best practices: installer diagnostics should
  provide actionable failure detail and preserve the ability to repair or roll
  back failed installs.
  https://learn.microsoft.com/en-us/windows/win32/msi/windows-installer-best-practices
- Microsoft rollback custom actions: unsuccessful installs must restore the
  original state.
  https://learn.microsoft.com/en-us/windows/win32/msi/rollback-custom-actions
- AWS ECS rollback guidance: health checks are a deployment decision point, and
  deployment state should be inspected during rollout/rollback.
  https://aws.amazon.com/blogs/compute/automating-rollback-of-failed-amazon-ecs-deployments/

Applied decisions:

- Keep fail-closed rollback after a bounded deadline.
- Make the default deadline match observed real startup cost with margin
  (600 seconds vs. an observed about 268 seconds).
- Print periodic process/socket/startup-log evidence instead of silently
  sleeping.
- Add an early daemon startup marker before storage/Calyx open so missing
  progress is visible in logs.

## Sources Of Truth

- Setup stdout/stderr logs:
  `%LOCALAPPDATA%\synapse\logs\issue-1731-1733-happy-setup.*.log` and
  `%LOCALAPPDATA%\synapse\logs\issue-1731-edge-*.log`.
- Daemon startup log:
  `%LOCALAPPDATA%\synapse\logs\synapse.log.2026-07-17`.
- Installed binary:
  `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`.
- Process/socket:
  `Get-Process synapse-mcp` and `Get-NetTCPConnection 127.0.0.1:7700`.
- Runtime health:
  authenticated `GET http://127.0.0.1:7700/health?detail=compact` and the real
  wired `mcp__synapse.health`.

## Happy Path - Slow Live Startup Completes Inside New Deadline

Synthetic input:

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1731
```

Expected output: setup prints the 600-second deadline, reports initial
connection-refused progress while the daemon has not bound yet, waits for the
installed daemon to bind after real storage/Calyx startup, and leaves a healthy
daemon.

Before:

```text
time=2026-07-17T14:15:17.6110669-05:00
installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
daemon_pid=70960
listener=127.0.0.1:7700 owner=70960
```

After:

```text
setup_exit=0
setup_log=%LOCALAPPDATA%\synapse\logs\issue-1731-1733-happy-setup.stdout.log
health_timeout_line=[synapse-setup] Installed daemon health timeout seconds=600
first_progress_line=[synapse-setup] SYNAPSE_INSTALL_HEALTH_PROGRESS attempt=1 remaining_s=598 last_health_error=No connection could be made because the target machine actively refused it. (127.0.0.1:7700)
installed_health_line=[synapse-setup] Daemon OK: pid=60584 version=0.1.0 db=C:\Users\hotra\AppData\Local\synapse\db-daemon
installed_sha256=A2C29E8FF1D68C62B28D7E915C7890D96E8317DADC6571A259DE6A2B849B4C3A
daemon_pid=60584
listener=127.0.0.1:7700 owner=60584
mcp_health_ok=true pid=60584 tool_count=40 storage=ok calyx_vault=ok chrome_bridge=ok
```

Daemon startup log readback after the fix:

```text
2026-07-17T19:30:01.562056Z MCP_DAEMON_STORAGE_AND_CALYX_OPEN_START db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
2026-07-17T19:34:35.955824Z MCP_DAEMON_STORAGE_AND_CALYX_OPENED db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
2026-07-17T19:34:36.521270Z MCP_HTTP_BIND_NORMAL bind=127.0.0.1:7700
```

## Edge 1 - Existing Larger Compiler Stack With Slow Startup

Synthetic input:

```text
parent_RUST_MIN_STACK=16777216
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1731
```

Expected output: setup keeps the 600-second installed-health deadline and still
waits through live startup.

Before/after readback:

```text
before_pid=60584 before_listener_owner=60584
setup_exit=0
health_timeout_line=[synapse-setup] Installed daemon health timeout seconds=600
installed_health_line=[synapse-setup] Daemon OK: pid=24800 version=0.1.0 db=C:\Users\hotra\AppData\Local\synapse\db-daemon
after_pid=24800 after_listener_owner=24800
mcp_health_ok=true pid=24800 tool_count=40 storage=ok calyx_vault=ok chrome_bridge=ok
```

Log readback:

```text
2026-07-17T19:30:01.562056Z MCP_DAEMON_STORAGE_AND_CALYX_OPEN_START db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
2026-07-17T19:34:35.955824Z MCP_DAEMON_STORAGE_AND_CALYX_OPENED db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
2026-07-17T19:34:36.521270Z MCP_HTTP_BIND_NORMAL bind=127.0.0.1:7700
```

## Edge 2 - Too-Small Compiler Stack With Slow Startup

Synthetic input:

```text
parent_RUST_MIN_STACK=1
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1731
```

Expected output: setup raises the stack and still keeps the installed-health
wait bounded by 600 seconds.

Before/after readback:

```text
before_pid=24800 before_listener_owner=24800
setup_exit=0
health_timeout_line=[synapse-setup] Installed daemon health timeout seconds=600
installed_health_line=[synapse-setup] Daemon OK: pid=39352 version=0.1.0 db=C:\Users\hotra\AppData\Local\synapse\db-daemon
after_pid=39352 after_listener_owner=39352
mcp_health_ok=true pid=39352 tool_count=40 storage=ok calyx_vault=ok chrome_bridge=ok
```

Log readback:

```text
2026-07-17T19:42:24.638048Z MCP_DAEMON_STORAGE_AND_CALYX_OPEN_START db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
2026-07-17T19:47:53.345657Z MCP_DAEMON_STORAGE_AND_CALYX_OPENED db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
2026-07-17T19:47:53.965970Z MCP_HTTP_BIND_NORMAL bind=127.0.0.1:7700
```

## Edge 3 - Malformed Compiler Stack With Slow Startup

Synthetic input:

```text
parent_RUST_MIN_STACK=not-a-number
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1731
```

Expected output: setup raises the malformed stack value, uses the same
600-second installed-health deadline, and leaves the installed daemon healthy.

Before/after readback:

```text
before_pid=39352 before_listener_owner=39352
setup_exit=0
health_timeout_line=[synapse-setup] Installed daemon health timeout seconds=600
installed_health_line=[synapse-setup] Daemon OK: pid=85360 version=0.1.0 db=C:\Users\hotra\AppData\Local\synapse\db-daemon
after_pid=85360 after_listener_owner=85360
mcp_health_ok=true pid=85360 tool_count=40 storage=ok calyx_vault=ok chrome_bridge=ok
```

Log readback:

```text
2026-07-17T19:56:21.476921Z MCP_DAEMON_STORAGE_AND_CALYX_OPEN_START db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
2026-07-17T20:02:43.709054Z MCP_DAEMON_STORAGE_AND_CALYX_OPENED db_path=C:\Users\hotra\AppData\Local\synapse\db-daemon
2026-07-17T20:02:44.269428Z MCP_HTTP_BIND_NORMAL bind=127.0.0.1:7700
```

## Final Evidence Of Success

```text
installed_sha256=FAD10EF1A7F0BF19830536846F1EFC69A5207691BE352DF0013FE7A1C045B0E6
daemon_pid=85360
listener=127.0.0.1:7700 owner=85360
authenticated_http_health_ok=true pid=85360 tool_count=241 storage=ok calyx_vault=ok chrome_bridge=ok
wired_mcp_health_ok=true pid=85360 tool_count=40 tool_surface_sha256=d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb
startup_markers_present=true
latest_installed_startup_sequence=
  MCP_DAEMON_STORAGE_AND_CALYX_OPEN_START at 2026-07-17T19:56:21.476921Z
  MCP_DAEMON_STORAGE_AND_CALYX_OPENED at 2026-07-17T20:02:43.709054Z
  MCP_HTTP_BIND_NORMAL at 2026-07-17T20:02:44.269428Z
```
