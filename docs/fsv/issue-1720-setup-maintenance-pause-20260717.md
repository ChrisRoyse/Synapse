# Manual FSV Closeout: Issue #1720 Setup Maintenance Pause Health Preflight

Date: 2026-07-17

Issue: https://github.com/ChrisRoyse/Synapse/issues/1720

## Result

Accepted. `scripts/synapse-setup.ps1` no longer treats one slow `/health`
preflight as the authoritative failure for Chrome bridge maintenance pause.
Setup retries health for diagnostics, then uses the maintenance-pause POST as
the fail-closed acknowledgement gate. A real active Chrome bridge can now
acknowledge pause even when the health snapshot is slow or unreadable.

No automated tests, FSV harnesses, benchmarks, mocks, CI, or GitHub Actions were
created or run. The commands listed here are structural checks or manual
Source-of-Truth readbacks only.

## Root Cause

During #1665 daemon install, `scripts/synapse-setup.ps1 -ForceRestart` failed
before binary handoff with:

```text
SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_HEALTH_FAILED
```

Physical readback at failure time showed the old daemon still owned the bind and
the scheduled task had been disabled during supervisor handoff suspension.
Daemon logs showed `/health` activity but no authoritative maintenance-pause
POST before setup exited.

The structural bug was authority inversion: `/health` is a broad subsystem
snapshot and can exceed a short timeout under real daemon/Chrome load. The
Chrome bridge maintenance-pause POST is the real acknowledgement gate because it
requires the active extension/native-host path to persist pause state and close
the WebSocket before restart.

## Implementation

`Request-SynapseChromeBridgeMaintenancePause` now:

- retries `/health` with `4`, `8`, then `12` second timeouts;
- records health attempts in the returned diagnostic object;
- still fails closed if health is readable but structurally missing the
  `chrome_bridge` subsystem/status;
- skips only when readable health or the POST says there is no active host;
- proceeds to the POST if health is unreadable after retries;
- treats the POST acknowledgement as the authoritative maintenance-pause gate;
- parses POST 503 bodies so explicit `no_active_chrome_bridge_host` remains a
  skip, while all other POST failures remain fatal.

## Research Used

Research was done after identifying the root cause, using the same setup
best-practice direction as the adjacent Chrome bridge maintenance work:

- Kubernetes probe docs for separating liveness/readiness/startup and keeping
  readiness cheap:
  https://kubernetes.io/docs/concepts/workloads/pods/probes/
- Microsoft PowerShell `Invoke-RestMethod` semantics for explicit HTTP timeout
  handling:
  https://learn.microsoft.com/powershell/module/microsoft.powershell.utility/invoke-restmethod

Takeaway applied here: diagnostic health checks should not replace the
operation-specific acknowledgement. The side-effecting maintenance-pause POST is
the only source that can prove the bridge actually accepted pause.

## Source Of Truth

Runtime SoTs:

```text
daemon_pid=74140
bind=127.0.0.1:7700
exe=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
command="C:\Users\hotra\.cargo\bin\synapse-mcp.exe" --mode http --bind 127.0.0.1:7700 --db C:\Users\hotra\AppData\Local\synapse\db-daemon --profile-dir C:\Users\hotra\.cargo\bin\profiles --log-level info
binary_sha256=2DFB163A1A4FA3AA1B54BDD0A552384DCF51788BEFA07128AEFAC6E7ED8D78CF
socket=127.0.0.1:7700 LISTEN owner=74140
```

Setup/bridge SoTs:

```text
script=scripts\synapse-setup.ps1
daemon_log=C:\Users\hotra\AppData\Local\synapse\logs\synapse.log.2026-07-17
candidate_root=C:\Users\hotra\AppData\Local\synapse\logs\setup-candidates\candidate-20260717T093045312Z-54076
stale_schema_handoff=C:\Users\hotra\AppData\Local\synapse\codex-restart-handoffs\codex-restart-handoff-73344-20260717T093342066Z.json
```

## Before State

Issue body recorded the failing state:

```text
old_daemon_pid=87184
bind=127.0.0.1:7700
setup_failed_code=SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_HEALTH_FAILED
scheduled_task=SynapseMcpDaemon Disabled after supervisor handoff suspension
missing_event=no maintenance-pause POST before setup exited
```

This was not an acceptable failure mode because a slow diagnostic health
snapshot prevented the authoritative bridge pause attempt.

## Trigger

After patching the setup script, setup was run for the #1665 repo-built daemon
install:

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1 -SourceDir C:\code\Synapse -ForceRestart
```

The setup run eventually exited non-zero only because the current Codex process
had stale in-process `episode` callable metadata:

```text
reason_code=SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE
current_codex_pid=73344
daemon_pid=74140
daemon_tool_count=40
daemon_tool_surface_sha256=7baef0742b0aacbb2a838a301a3ef25af3175468b15d831f6aa90b88dfd7b776
callable_schema_changed=episode
```

That stale Codex process handoff is expected D4 transport state and is not the
#1720 bug. The daemon install and Chrome bridge pause path completed before
that handoff.

## After State

Daemon log readback proved the real bridge pause POST path ran:

```text
2026-07-17T09:31:24.721829Z CHROME_DEBUGGER_COMMAND_QUEUED
host_id=chrome-native-0-1784279185997
command_kind=maintenancePauseReconnect

2026-07-17T09:31:24.722389Z CHROME_DEBUGGER_COMMAND_DELIVERED
host_id=chrome-native-0-1784279185997
command_kind=maintenancePauseReconnect

2026-07-17T09:31:24.727325Z CHROME_DEBUGGER_RESPONSE_ACCEPTED
host_id=chrome-native-0-1784279185997
command_kind=maintenancePauseReconnect
response_ok=true
readback={"bridge_build_id":"synapse-chrome-bridge-2026-07-16-maintenance-alarm-resume-v1","extension_id":"leoocgnkjnplbfdbklajepahofecgfbk","host_id":"chrome-native-0-1784279185997","pause_ms":720000,"pause_until_unix_ms":1784281404723,"reason":"install_binary","reconnect_suppressed":true}

2026-07-17T09:31:24.728454Z CHROME_DEBUGGER_DIRECT_HTTP_WS_DISCONNECTED
detail=maintenance pause acknowledged; daemon closing direct HTTP WebSocket before restart
```

Installed daemon readback:

```text
process_id=74140
process_name=synapse-mcp
path=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
start_time=2026-07-17 04:32:40 America/Chicago
binary_sha256=2DFB163A1A4FA3AA1B54BDD0A552384DCF51788BEFA07128AEFAC6E7ED8D78CF
socket=127.0.0.1:7700 LISTEN owner=74140
```

Real MCP health readback:

```text
ok=true
pid=74140
storage.status=ok
storage.storage_backend=calyx
chrome_bridge.status=ok
chrome_bridge.host_count=1
tool_count=40
tool_surface_sha256=7baef0742b0aacbb2a838a301a3ef25af3175468b15d831f6aa90b88dfd7b776
```

## Edge Audit

### Edge 1: Health Slow Or Unreadable

Before the fix, this was the failing case:

```text
health_timeout=4s
setup_result=SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_HEALTH_FAILED
maintenance_pause_post_attempted=false
```

After the fix, unreadable health records attempts and proceeds to the
maintenance-pause POST because the POST is the authoritative ack gate.

### Edge 2: Explicit No Active Host

Readable health with `no_active_chrome_bridge_host`, or POST `503` whose parsed
detail says `no_active_chrome_bridge_host`, returns:

```text
Ok=true
Skipped=true
Code=SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_SKIPPED_NO_ACTIVE_HOST
```

This avoids a false hard failure when there is no bridge to pause.

### Edge 3: Real Active Host Ack

The active host case was manually exercised by the setup run:

```text
host_id=chrome-native-0-1784279185997
command_kind=maintenancePauseReconnect
response_ok=true
pause_ms=720000
reconnect_suppressed=true
websocket_disconnected_after_ack=true
```

The daemon was then replaced and the new repo-built daemon owned the configured
listener.

## Structural Checks

Supporting structural checks run locally after implementation:

```text
cargo fmt --all
cargo check --workspace
git diff --check
```

Final `cargo fmt --all --check` and `cargo clippy --workspace --all-targets`
were run before commit. These are compile/lint checks only, not FSV.

