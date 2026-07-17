# Issue #1729 FSV - install-health rollback drill

Date: 2026-07-17
Agent: Codex
Issue: https://github.com/ChrisRoyse/Synapse/issues/1729

## Result

Accepted for the setup rollback scope. `scripts/synapse-setup.ps1` now has an
operator-only manual rollback drill that builds and preflights a real candidate,
installs it, starts it through the real scheduled-task supervisor, rejects the
installed `/health` gate, and then proves rollback by re-reading the installed
binary, daemon process, listener, supervisor ledger, `/health`, and Chrome bridge
state.

No automated tests, FSV harnesses, benchmarks, or CI were created or run.
Compile/lint commands are structural checks only and are not FSV.

## Root Cause

The setup script had rollback behavior for install-health failure, but no
deterministic physical drill that forced the failure after a real candidate had
been installed and started. That meant the dangerous path was only exercised by
incidental failures, and there was no clean way to prove all rollback branches:
candidate stop, previous-binary restore, launcher restoration, active Chrome
bridge maintenance-pause ACK, unacknowledged pause, final daemon restart, and
post-rollback readback.

The fix adds `-ManualInstallHealthRollbackProbe` plus
`-ManualInstallHealthRollbackPauseMode normal|require_active_ack|force_unacknowledged`.
The probe fails closed unless it is a real `-ForceRestart` install with a freshly
built candidate and an existing installed binary to restore. It exits fail-loud
after rollback instead of masking the rejected candidate as success.

## Best-Practice Research Inputs

Exa and browser research were used before finalizing the fix:

- Microsoft Windows Installer best practices: custom actions that change system
  state need explicit rollback actions, and installation diagnostics should
  write useful error details.
  https://learn.microsoft.com/en-us/windows/win32/msi/windows-installer-best-practices
- Microsoft rollback custom actions: rollback exists to restore original state
  after an unsuccessful install and must handle interrupted operations.
  https://learn.microsoft.com/en-us/windows/win32/msi/rollback-custom-actions
- AWS ECS rollback guidance: health-check failures should trigger rollback, and
  the result must be verified on the service state after the rollback workflow.
  https://aws.amazon.com/blogs/compute/automating-rollback-of-failed-amazon-ecs-deployments/

Applied decisions:

- Preserve the previous installed binary before installing the candidate.
- Fail loudly with candidate hash, installed hash, listener snapshot, process
  snapshot, and rollback hash.
- Restore the normal hidden launcher before both rollback stop and rollback
  start so the scheduled-task supervisor cannot restart the manual probe mode.
- Verify success by reading the physical Source of Truth after the rollback, not
  by trusting the setup process return value.

## Sources Of Truth

- Installed binary: `C:\Users\hotra\.cargo\bin\synapse-mcp.exe`.
- Scheduled task: `SynapseMcpDaemon`.
- Daemon process/socket: `Win32_Process` for `synapse-mcp.exe` and
  `Get-NetTCPConnection 127.0.0.1:7700`.
- Runtime health: authenticated `GET http://127.0.0.1:7700/health?detail=compact`
  and the real wired `mcp__synapse.health` tool.
- Setup logs:
  `%LOCALAPPDATA%\synapse\logs\issue-1729-*-setup.*.log`.
- Supervisor ledger:
  `%LOCALAPPDATA%\synapse\logs\daemon-supervisor-current.json` and
  `daemon-supervisor-events.jsonl`.
- Daemon lifecycle ledger:
  `%LOCALAPPDATA%\synapse\db-daemon\daemon-tool-last.json`.

## Happy Path - Active Chrome Bridge ACK Rollback

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

Expected output: setup installs a real candidate, waits until the candidate has
an active Chrome bridge host, rejects the candidate health gate, receives a real
maintenance-pause ACK, stops the candidate, restores the previous binary, starts
the restored daemon, verifies `/health`, and exits with failure after printing
the rollback evidence.

Before:

```text
time=2026-07-17T13:33:37.0990573-05:00
installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
daemon_pid=24036
listener_owner=24036
health_ok=true
chrome_bridge=ok host_count=1
```

Trigger/readback evidence:

```text
log=%LOCALAPPDATA%\synapse\logs\issue-1729-active-ack-required-setup.stdout.log
candidate_staged_sha256=66616241222464377A635C7C620D070212BC0CAD1777FA0A5761C70F981C5344
preflight_pid=68256 bind=127.0.0.1:64184 tool_count=40
installed_candidate_sha256=66616241222464377A635C7C620D070212BC0CAD1777FA0A5761C70F981C5344
candidate_pid=20344
candidate_health_before_ack=chrome_bridge unavailable, then active host observed
rollback_pause=SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_ACK
rollback_pause_pid=20344
candidate_exit=child_pid=20344 exit_code=1
rollback_pid=74288
rollback_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
rollback_chrome_bridge_status=ok host_count=1
```

After separate SoT read:

```text
time=2026-07-17T13:43:48.4689330-05:00
installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
task_state=Running
daemon_pid=74288
listener=127.0.0.1:7700 owner=74288
setup_lock=<missing>
supervisor_state=running child_pid=74288
health_ok=true
http=ok storage=ok calyx_vault=ok chrome_bridge=ok host_count=1
mcp_tool_count=40
mcp_tool_surface_sha256=d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb
```

## Edge 1 - No Active Chrome Bridge Host

Synthetic input:

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1729
  -ManualInstallHealthRollbackProbe
```

Expected output: setup rejects the critical-ready candidate before Chrome bridge
has an active host, skips maintenance pause with an explicit no-active-host
diagnostic, rolls back, waits for the restored daemon to regain the bridge, and
exits fail-loud with rollback readback.

Evidence:

```text
log=%LOCALAPPDATA%\synapse\logs\issue-1729-startup-order-normal-setup.stdout.log
before_installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
before_daemon_pid=39532 listener_owner=39532 chrome_bridge=ok host_count=1
candidate_sha256=107490C94813AFB01E6DDDC9D7D7B43330CC59327A64588E79A9323817E13B68
candidate_pid=28296
candidate_listener_owner=28296
rollback_pause=SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_SKIPPED_NO_ACTIVE_HOST
candidate_exit=child_pid=28296 exit_code=1
rollback_pid=24036
after_installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
after_listener_owner=24036
after_health_ok=true chrome_bridge=ok host_count=1
```

## Edge 2 - Forced Unacknowledged Maintenance Pause

Synthetic input:

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1729
  -ManualInstallHealthRollbackProbe
  -ManualInstallHealthRollbackPauseMode force_unacknowledged
```

Expected output: setup reads candidate health, intentionally returns an explicit
manual unacknowledged-pause diagnostic, then rollback force-stops the exact
candidate PID if graceful stop does not complete and verifies the restored
daemon.

Evidence:

```text
log=%LOCALAPPDATA%\synapse\logs\issue-1729-active-forced-unack-corrected-rerun-setup.log
before_installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
before_daemon_pid=66724 listener_owner=66724 chrome_bridge=ok host_count=1
candidate_sha256=F62AD1107DB9B1A7F8B2D5E6FFF1CDCC1FC92EEAE6425B3A3906DD567745E4CD
candidate_pid=86360
rollback_pause=SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_MANUAL_UNACKNOWLEDGED_PROBE
force_stop_exact_pid=86360
rollback_pid=80088
after_installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
after_listener_owner=80088
after_health_ok=true chrome_bridge=ok host_count=1
```

## Edge 3 - Structurally Invalid Probe Flags

Synthetic input:

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1
  -SourceDir C:\code\Synapse
  -ForceRestart
  -SkipClientWiring
  -ActiveIssue 1729
  -ManualInstallHealthRollbackPauseMode require_active_ack
```

Expected output: fail during setup preflight because pause mode changes rollback
behavior and is only valid with `-ManualInstallHealthRollbackProbe`. The running
daemon must not be stopped, rebuilt, replaced, or rebound.

Before/after readback:

```text
log=%LOCALAPPDATA%\synapse\logs\issue-1729-invalid-pause-without-probe.stderr.log
exit_code=1
fatal_code=SYNAPSE_MANUAL_INSTALL_HEALTH_ROLLBACK_PAUSE_MODE_WITHOUT_PROBE
before_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
after_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
before_pid=74288 after_pid=74288
before_listener_owner=74288 after_listener_owner=74288
setup_lock=<missing>
maintenance_lock_state=failed released_at_utc=2026-07-17T18:44:47.3247196Z
```

## Final Evidence Of Success

The installed daemon after all probes:

```text
installed_sha256=AAF28F12F7504FB1A3502DA201CF8B73265C047F19DDD662850B7408C5D7E345
process=74288 C:\Users\hotra\.cargo\bin\synapse-mcp.exe --mode http --bind 127.0.0.1:7700 --db C:\Users\hotra\AppData\Local\synapse\db-daemon
listener=127.0.0.1:7700 owner=74288
scheduled_task=SynapseMcpDaemon state=Running
supervisor=running child_pid=74288
http_health_ok=true pid=74288 tool_count=241 chrome_bridge=ok host_count=1
wired_mcp_health_ok=true pid=74288 tool_count=40 chrome_bridge=ok host_count=1
```
