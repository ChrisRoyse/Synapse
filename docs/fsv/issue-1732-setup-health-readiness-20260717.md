# Issue #1732 FSV - setup health readiness cannot hang on early listener

Date: 2026-07-17
Agent: Codex
Issue: https://github.com/ChrisRoyse/Synapse/issues/1732

## Result

Accepted for the daemon startup readiness scope. `synapse-mcp --mode http` no
longer binds the production TCP listener before eager storage, Calyx vault, and
activity recorder startup preflights complete. If those preflights are still in
progress, setup now sees connection refused instead of a bound-but-unserved
socket. If a preflight fails, the process exits before binding.

No automated tests, FSV harnesses, benchmarks, or CI were created or run.
Compile/lint commands are structural checks only and are not FSV.

## Root Cause

In `crates/synapse-mcp/src/http/transport.rs`, the HTTP listener was bound
before `start_http_runtime` and before eager storage/Calyx/activity-recorder
startup preflights. Setup health polling used the TCP listener as physical proof
that the daemon had reached HTTP readiness, but requests could still hang because
the listener existed before any request-serving runtime existed.

The fix moves `bind_http_listener(addr).await` until after the preflights. A
daemon that is not ready to answer `/health` no longer owns the production bind.

## Best-Practice Research Inputs

The same rollback/deployment research used for #1729 applies here:

- Microsoft rollback custom actions: unsuccessful installs should restore the
  original state and rollback logic must handle interrupted work.
  https://learn.microsoft.com/en-us/windows/win32/msi/rollback-custom-actions
- Microsoft Windows Installer best practices: write actionable diagnostics and
  thoroughly validate install packages before deployment.
  https://learn.microsoft.com/en-us/windows/win32/msi/windows-installer-best-practices
- AWS ECS rollback guidance: health checks are the deployment decision point,
  and the service state must be inspected after rollback.
  https://aws.amazon.com/blogs/compute/automating-rollback-of-failed-amazon-ecs-deployments/

Applied decisions:

- Do not expose a listener until the daemon can actually serve `/health`.
- On failed startup preflight, exit with the original error and no listener.
- Keep setup fail-loud diagnostics that print candidate hash, listener snapshot,
  process snapshot, and rollback readback.

## Sources Of Truth

- Candidate binary:
  `%LOCALAPPDATA%\synapse\build-target\Synapse-a924ab647587\release\synapse-mcp.exe`.
- Production daemon:
  `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`, Task Scheduler row
  `SynapseMcpDaemon`, and TCP listener `127.0.0.1:7700`.
- Alternate-port invalid-startup probe: TCP listener `127.0.0.1:7777`, process
  PID, and stderr log.
- Setup logs:
  `%LOCALAPPDATA%\synapse\logs\issue-1729-startup-order-normal-setup.stdout.log`,
  `%LOCALAPPDATA%\synapse\logs\issue-1729-active-ack-required-setup.stdout.log`,
  and `%LOCALAPPDATA%\synapse\logs\issue-1732-invalid-db-startup.stderr.log`.
- Runtime health: authenticated `/health?detail=compact` and real wired
  `mcp__synapse.health`.

## Before Fix Evidence

During the first active-host rollback probe, setup observed a candidate process
that had already bound `127.0.0.1:7700`, but `/health` requests timed out while
startup preflights were still progressing.

```text
log=%LOCALAPPDATA%\synapse\logs\issue-1729-active-ack-setup.stdout.log
candidate_pid=59920
candidate_bind=127.0.0.1:7700
launch_utc=2026-07-17T18:06:35.4023492Z
exit_utc=2026-07-17T18:11:14.8800716Z
runtime_ms=279526
health_result=timeout while listener existed
```

That state made setup health gates ambiguous: a bound socket looked alive, but
there was no serving HTTP runtime to answer the health request.

## Happy Path - Delayed Startup Without Early Listener

Synthetic input:

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1729
  -ManualInstallHealthRollbackProbe
```

Expected output: setup starts the real candidate. While startup preflights are
not ready, `/health` fails fast with connection refused. Once the listener is
bound, `/health` answers with a real PID. The manual rollback probe then rejects
the candidate and rollback restores the previous daemon.

Readback:

```text
candidate_sha256=107490C94813AFB01E6DDDC9D7D7B43330CC59327A64588E79A9323817E13B68
candidate_pid=28296
health_attempt_1=No connection could be made because the target machine actively refused it. (127.0.0.1:7700)
health_attempt_2=No connection could be made because the target machine actively refused it. (127.0.0.1:7700)
health_after_bind=Daemon OK: pid=28296
rollback_pid=24036
rollback_health_ok=true chrome_bridge=ok host_count=1
```

## Edge 1 - Active Chrome Bridge Startup Delay

Synthetic input:

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1729
  -ManualInstallHealthRollbackProbe
  -ManualInstallHealthRollbackPauseMode require_active_ack
```

Expected output: setup must continue to distinguish "not yet listening" from
"listening and serving health", then wait until the candidate reports an active
Chrome bridge host before rejecting health.

Readback:

```text
candidate_sha256=66616241222464377A635C7C620D070212BC0CAD1777FA0A5761C70F981C5344
candidate_pid=20344
first_candidate_health=chrome_bridge unavailable
setup_action=waiting for active Chrome bridge before ACK edge
active_bridge_observed=true pid=20344
rollback_pause=SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_ACK
rollback_pid=74288
rollback_health_ok=true chrome_bridge=ok host_count=1
```

## Edge 2 - Invalid DB Path Exits Before Binding

Synthetic input:

```text
%LOCALAPPDATA%\synapse\build-target\Synapse-a924ab647587\release\synapse-mcp.exe
  --mode http
  --bind 127.0.0.1:7777
  --db %LOCALAPPDATA%\synapse\fsv\issue-1732\invalid-db-file\db-is-a-file
  --profile-dir %USERPROFILE%\.cargo\bin\profiles
  --log-level info
```

Expected output: because the DB path is a regular file, startup must fail with a
filesystem diagnostic before binding `127.0.0.1:7777`.

Before/after readback:

```text
candidate_sha256=66616241222464377A635C7C620D070212BC0CAD1777FA0A5761C70F981C5344
bad_db_is_file=true
before_listener_7777=<none>
spawned_pid=83084
exited_within_30s=true
exit_code=1
after_listener_7777=<none>
process_alive=false
stderr=synapse-mcp error: acquire daemon single-instance lock: failed to acquire daemon single-instance lock ... create db directory: Cannot create a file when that file already exists. (os error 183)
```

Production daemon readback immediately after the invalid-startup probe:

```text
production_pid=74288
production_listener=127.0.0.1:7700 owner=74288
production_health_ok=true
production_chrome_bridge=ok host_count=1
production_http_tool_count=241
```

## Edge 3 - Structurally Invalid Setup Probe Flags

Synthetic input:

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1729
  -ManualInstallHealthRollbackPauseMode require_active_ack
```

Expected output: setup must fail in preflight before touching the running daemon
or listener.

Readback:

```text
exit_code=1
fatal_code=SYNAPSE_MANUAL_INSTALL_HEALTH_ROLLBACK_PAUSE_MODE_WITHOUT_PROBE
before_installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
after_installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
before_pid=74288 after_pid=74288
before_listener_owner=74288 after_listener_owner=74288
```

## Final Evidence Of Success

```text
installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
production_pid=74288
listener=127.0.0.1:7700 owner=74288
scheduled_task=SynapseMcpDaemon state=Running
http_health_ok=true pid=74288 tool_count=241 chrome_bridge=ok host_count=1
wired_mcp_health_ok=true pid=74288 tool_count=40 chrome_bridge=ok host_count=1
invalid_startup_listener_7777=<none>
```
