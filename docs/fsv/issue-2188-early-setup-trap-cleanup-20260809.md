# Issue #2188 — early setup trap cleanup

Date: 2026-08-09 (UTC)

## Defect and Source of Truth

The real trigger was `scripts/synapse-setup.ps1` with an invalid audio
deployment contract. Before this change the intended
`SYNAPSE_AUDIO_DEPLOYMENT_CONTRACT_INVALID` error was preceded by a secondary
failure:

```text
SYNAPSE_DEPLOY_RESTART_AUTHORITY_RESTORE_TRAP_FAILED
The term 'Restore-SynapseDeployRestartAuthorityBestEffort' is not recognized
```

The authorities used for acceptance were the PowerShell process error stream,
the OS process table, the installed executable bytes, the Task Scheduler row,
the durable supervisor stop-request file, authenticated daemon health, and the
setup maintenance ledger. A return value alone was not accepted.

The final committed-checkout deployment is recorded in the stable GitHub
[deployment readback comment](https://github.com/ChrisRoyse/Synapse/issues/2188#issuecomment-5232725418).
That comment is updated after this document's commit so documenting the final
PID/hash does not itself advance HEAD and make build provenance stale.

## Root cause and research

The script's top-level `trap` was lexically above the audio-contract validation,
but PowerShell hoists traps across their complete scriptblock. The trap was
therefore active for the early error. The three cleanup declarations were
executed later in the file, so the cleanup command did not yet exist when the
trap invoked it.

The repository research-lane probe reported Exa MCP `live`: Exa 3.4.0
initialized, listed `web_search_exa`/`web_fetch_exa`, and completed a real
tools/call. Exa and the built-in web lane independently found the same Microsoft
Learn primary sources:

- [about_Trap](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_trap?view=powershell-7.6)
  states that traps are hoisted and apply to statements even before execution
  reaches the trap's lexical location.
- [about_Functions](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_functions?view=powershell-7.6)
  states that functions in script files must be defined before calls.
- [about_Try_Catch_Finally](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_try_catch_finally?view=powershell-7.6)
  describes guaranteed cleanup on terminating errors.

The complete deploy-restart-authority state and Set/Clear/Restore functions now
execute before the trap. The implementation is moved intact. There is no stub,
fallback, duplicate cleanup path, swallowed primary error, or reduced failure
severity. Later drain, continuation, and success paths call the same functions.

Final declaration order read directly from the source:

```text
1301 Set-SynapseDeployRestartAuthorityRevocation
1310 Clear-SynapseDeployRestartAuthorityRevocation
1315 Restore-SynapseDeployRestartAuthorityBestEffort
1358 trap
1445 SYNAPSE_AUDIO_DEPLOYMENT_CONTRACT_INVALID
```

The edited script SHA-256 before commit was
`3D2248B1697EA7B165039E1167F05011A0663E38CB92CD8D88A883716F3D1F10`.

## Before state

Immediately before the manual failure triggers:

```text
daemon_pid=16356
daemon_image=C:\Users\hotra\.cargo\bin\synapse-mcp.exe
daemon_sha256=683155234C3015763CE72C4AE63C1A8E24D4B3C8D72F70576C813CD1502B9309
daemon_build=33b40df64682
scheduled_task=SynapseMcpDaemon
scheduled_task_state=Running
stop_request=C:\Users\hotra\.cargo\bin\daemon-supervisor-stop-request.json
stop_request_exists=false
health_ok=true
```

The setup maintenance ledger already held the terminal record of the prior
post-install stale-Codex-schema verdict. Early parameter validation occurs
before lock acquisition, so the three triggers below were also required not to
replace or relabel that unrelated terminal record.

## Manual happy path and boundary audit

These were separate human-selected script invocations, not automated tests or a
probe harness.

### 1. Capture enabled without permission

Trigger:

```text
pwsh -File scripts\synapse-setup.ps1 -SourceDir C:\code\synapse \
  -ForceRestart -EnableAudio \
  -AllowedPermissions READ_EVENTS,READ_REFLEX,READ_PROFILE,READ_STORAGE,WRITE_STORAGE \
  -ActiveIssue 2188
```

Observed output and after state:

```text
exit=1
primary_code=SYNAPSE_AUDIO_DEPLOYMENT_CONTRACT_INVALID
enable_audio=True
read_audio_granted=False
secondary_restore_failure_count=0
daemon_pid_after=16356
daemon_sha256_after=683155234C3015763CE72C4AE63C1A8E24D4B3C8D72F70576C813CD1502B9309
task_state_after=Running
stop_request_exists_after=false
health_ok_after=true
```

### 2. Permission granted while capture is disabled

Trigger:

```text
pwsh -File scripts\synapse-setup.ps1 -SourceDir C:\code\synapse \
  -AllowedPermissions READ_AUDIO -ActiveIssue 2188
```

Observed output and independent state between edge 2 and edge 3:

```text
exit=1
primary_code=SYNAPSE_AUDIO_DEPLOYMENT_CONTRACT_INVALID
enable_audio=False
read_audio_granted=True
secondary_restore_failure_count=0
daemon_pid_after=16356
daemon_sha256_after=683155234C3015763CE72C4AE63C1A8E24D4B3C8D72F70576C813CD1502B9309
task_state_after=Running
stop_request_exists_after=false
```

### 3. Whitespace-only permission boundary

Trigger:

```text
pwsh -File scripts\synapse-setup.ps1 -SourceDir C:\code\synapse \
  -EnableAudio -AllowedPermissions '   ' -ActiveIssue 2188
```

Normalization produced an empty permission set and failed loudly:

```text
exit=1
primary_code=SYNAPSE_AUDIO_DEPLOYMENT_CONTRACT_INVALID
enable_audio=True
read_audio_granted=False
allowed_permissions=<empty>
secondary_restore_failure_count=0
daemon_pid_after=16356
health_pid_after=16356
health_ok_after=true
health_build_after=33b40df64682
daemon_sha256_after=683155234C3015763CE72C4AE63C1A8E24D4B3C8D72F70576C813CD1502B9309
task_state_after=Running
stop_request_exists_after=false
```

For this defect, the expected success is an early failure that preserves its
original diagnostic and performs an idempotent no-op cleanup when no restart
authority has yet been revoked. All three real paths achieved that exact state.
The full valid setup happy path is the final post-commit deployment linked at
the start of this record; it uses the matching audio/permission contract and
then independently reads the installed process and vault.

## Structural gates

`cargo check --workspace` exited 0. The canonical
`pwsh -File scripts/lint.ps1` exited 0 and reported:

```text
1355 Rust files; zero automated-test/bench/FSV executable surface
21 shared lint-contract lines matched
both toolchains 1.97.1
lock graph violations=0
root and calyx fmt/deny/clippy passed with -D warnings
public API=357; baseline=357
LINT OK: every gate passed in BOTH workspaces (root and calyx).
```

No automated test, CI job, mock data, worktree, branch, fallback, or FSV
harness was created or run.
