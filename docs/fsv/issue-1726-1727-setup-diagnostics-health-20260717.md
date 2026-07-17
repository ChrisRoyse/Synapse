# Issue 1726 / 1727 setup diagnostics and health FSV - 2026-07-17

## Research

- Exa and web research used Microsoft Learn:
  - https://learn.microsoft.com/en-us/windows/win32/wer/collecting-user-mode-dumps
  - https://learn.microsoft.com/en-us/shows/inside/c0000005
- Applied conclusion: `0xC0000005` is a native access-violation crash. Setup must preserve per-attempt build evidence and read WER LocalDumps state, because WER local dump collection is not enabled by default and crash artifacts may be absent.

## Changes

- `scripts/synapse-setup.ps1`
  - Preserves `setup-build-diagnostics.json` across later successful retries.
  - Writes immutable per-attempt build failure archives under `%LOCALAPPDATA%\synapse\logs\setup-build-failures`.
  - Classifies `0xC0000005` as `SYNAPSE_RELEASE_BUILD_TOOLCHAIN_ACCESS_VIOLATION`.
  - Adds Rust toolchain and WER crash-dump readbacks to build failure diagnostics.
  - Allows setup install readiness to treat storage `maintenance` as ready.
  - Hardens install-health rollback so restored daemon readiness uses the same critical-subsystem and Chrome bridge readback path as successful install.
- `crates/synapse-mcp/src/server/health.rs`
  - Reports contended non-storage runtime locks as `busy`, not hard `error`.
  - For storage health, when the reflex runtime lock is busy, reads the daemon storage handle and maintenance/pressure Source of Truth from `M3State` instead of failing the storage subsystem.

## Manual FSV Source Of Truth

- Build failure evidence:
  - `%LOCALAPPDATA%\synapse\logs\setup-build-diagnostics.json`
  - `%LOCALAPPDATA%\synapse\logs\setup-build-failures\*.diagnostics.json`
  - `%LOCALAPPDATA%\synapse\logs\setup-build-failures\*.build.log`
  - `%LOCALAPPDATA%\synapse\logs\setup-build.log`
- Installed daemon evidence:
  - OS process table for `synapse-mcp.exe`
  - TCP listener on `127.0.0.1:7700`
  - `%USERPROFILE%\.cargo\bin\synapse-mcp.exe` SHA-256
  - `%APPDATA%\synapse\codex-tool-surface.json`
  - real `mcp__synapse.health`, `mcp__synapse.storage`
  - `%LOCALAPPDATA%\synapse\db-daemon\daemon-tool-last.json`

## Issue 1726 Evidence

Before trigger:

- `setup-build-diagnostics.json`: absent.
- `setup-build-failures`: absent.
- `setup-build.log`: existed, success tail only.
- Installed daemon PID: `29796`.
- Installed hash: `5BA2E630307C895EF9BB473048D644714139576F0D0C15297AE485D30CC6A747`.

Edge 1 trigger:

- Command environment: `RUSTC_WRAPPER=C:\does-not-exist\synapse-rustc-wrapper-1726-edge1.exe`.
- Setup failed closed with `SYNAPSE_RELEASE_BUILD_COMPILER_FAILED`, exit `101`.
- Artifact hash readback remained `5BA2E630307C895EF9BB473048D644714139576F0D0C15297AE485D30CC6A747`.
- Archive written: `release-build-20260717T143605701Z-pid70776.*`.
- WER recent dump count: `0`.

Edge 1 after-read:

- Current diagnostics existed.
- Diagnostic archive existed.
- Log archive existed.
- Toolchain readback rows: `5`.

Edge 2 trigger:

- Command environment: `RUSTC_WRAPPER=C:\does-not-exist\synapse-rustc-wrapper-1726-edge2.exe`.
- Setup failed closed with `SYNAPSE_RELEASE_BUILD_COMPILER_FAILED`, exit `101`.
- Archive written: `release-build-20260717T143647705Z-pid79864.*`.

Edge 2 after-read:

- Current diagnostics pointed at `release-build-20260717T143647705Z-pid79864`.
- Archive directory contained 4 files: 2 diagnostics and 2 logs.

Happy path trigger:

- Command: `pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\synapse-setup.ps1 -SourceDir C:\code\Synapse -ForceRestart -SkipClientWiring -ActiveIssue 1727`.
- Setup completed.
- Installed PID after latest run: `62524`.
- Installed hash after latest run: `39CBF166D7197E12B0C86B028A3695903D538C80EF75A3D44D4874932F543492`.

Happy path after-read:

- `setup-build.log` contained the successful release build.
- `setup-build-diagnostics.json` still existed and still pointed at `release-build-20260717T143647705Z-pid79864`.
- Archive directory still contained the 4 failure files:
  - `release-build-20260717T143605701Z-pid70776.build.log`
  - `release-build-20260717T143605701Z-pid70776.diagnostics.json`
  - `release-build-20260717T143647705Z-pid79864.build.log`
  - `release-build-20260717T143647705Z-pid79864.diagnostics.json`

## Issue 1727 Evidence

Before fix, actual failure observed:

- Setup installed candidate hash `DEF8AE9ED3FCD4511C96222857C8F885BC0102099E96553F223FCF5D88ECFDB6`.
- Installed daemon PID: `80888`.
- Setup install health timed out for 180 seconds.
- Rollback failed at `SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_REQUEST_FAILED`.
- Real `mcp__synapse.health(detail=full)` after failure:
  - `ok=false`
  - `storage.status=error`
  - `storage.detail=reflex storage runtime lock is busy; health is fail-closed and does not wait behind in-flight work`
  - `reflex.status=error`
  - `reflex.detail=reflex runtime lock is busy; health is fail-closed and does not wait behind in-flight work`
  - `daemon_lifecycle.in_flight_count=2`

Happy path after fix:

- Real setup command completed with exit `0`.
- Candidate preflight passed with `tool_count=40`, `tool_surface_sha256=d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb`.
- Chrome maintenance pause ack was observed for the normal install drain.
- Old PID `54016` exited through authenticated graceful shutdown.
- New installed daemon PID: `62524`.
- Installed hash: `39CBF166D7197E12B0C86B028A3695903D538C80EF75A3D44D4874932F543492`.
- `%APPDATA%\synapse\codex-tool-surface.json` readback:
  - `tool_count=40`
  - `tool_surface_sha256=d5f6dca7809adb78a024aedf756a86c95d7dab4b146bf99155ae27e54ec3e8fb`
  - `daemon_pid=62524`

Separate Source-of-Truth readbacks:

- OS process table: one `synapse-mcp.exe`, PID `62524`, path `%USERPROFILE%\.cargo\bin\synapse-mcp.exe`.
- TCP listener: `127.0.0.1:7700`, owning process `62524`.
- Real `mcp__synapse.health(detail=compact)` after settle:
  - `ok=true`
  - `pid=62524`
  - `storage.status=ok`
  - `reflex.status=ok`
  - `chrome_bridge.status=ok`
  - `tool_count=40`
- Real `mcp__synapse.storage(operation=summary)`:
  - `readback_source_of_truth=calyx summary cf_count=17 pressure=Normal`
  - `CF_KV=165673`
  - `CF_TIMELINE=248647`
  - `storage_backend=calyx`

Edge 1, write-gated storage mutation without write grant:

- Before: `daemon-tool-last.json` showed `tool=profile`, `operation=status`, `status=ok`.
- Trigger: real `mcp__synapse.storage(operation=gc_once, cf_name=CF_MODEL_CACHE, run_id=issue-1727-edge-denied-no-write-grant)`.
- Expected: fail closed, no mutation accepted.
- Actual tool error: `TOOL_PROFILE_POLICY_DENIED`.
- After `daemon-tool-last.json`:
  - `tool=storage`
  - `operation=gc_once`
  - `status=error`
  - `error.data.code=TOOL_PROFILE_POLICY_DENIED`
- After health: `ok=true`, storage/reflex/Chrome all `ok`.

Edge 2, structurally invalid storage payload:

- Trigger: real `mcp__synapse.storage(operation=summary)` with both `summary` and `gc_once` payloads.
- Expected: fail closed with parameter error.
- Actual tool error: `TOOL_PARAMS_INVALID`, `extra_payloads=["gc_once"]`.
- After `daemon-tool-last.json`:
  - `tool=storage`
  - `operation=summary`
  - `status=error`
  - `error.data.code=TOOL_PARAMS_INVALID`
- After health: `ok=true`, storage/reflex/Chrome all `ok`.

Edge 3, empty read-only inspect payload:

- Trigger: real `mcp__synapse.storage(operation=inspect, inspect={})`.
- Expected: no synthetic data; read physical Calyx CF metadata.
- Actual:
  - `readback_source_of_truth=calyx CF rows=17 pressure=Normal`
  - `CF_MODEL_CACHE=0`
  - `CF_KV=165673`
  - `CF_TIMELINE=248649`
  - `storage_backend=calyx`
- After `daemon-tool-last.json`:
  - `tool=storage`
  - `operation=inspect`
  - `status=ok`
- After health: `ok=true`, storage/reflex/Chrome all `ok`.

## Caveat

The original install-health busy-lock failure no longer reproduced after the health fix. The rollback hardening was parser-checked and the normal maintenance pause/shutdown path was physically exercised during setup, but the new `AllowUnacknowledgedChromeBridgePauseForRollback` branch was not physically triggered. Do not use that branch as closure evidence without a real install-health-failed rollback trigger. Follow-up: https://github.com/ChrisRoyse/Synapse/issues/1729.

## Structural Checks

- `cargo fmt --all --check`: passed.
- `cargo check -p synapse-mcp`: passed.
- PowerShell parser check for `scripts\synapse-setup.ps1`: passed.
- No new automated tests, benches, FSV harnesses, or FSV scripts were added.
