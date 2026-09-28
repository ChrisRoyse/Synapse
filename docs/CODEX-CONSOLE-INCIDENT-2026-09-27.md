# Codex console flashes and Synapse agent-spawn policy

Issue: [#2266](https://github.com/ChrisRoyse/Synapse/issues/2266).

**Follow-up status:** the operator reported continued flashes, including on Enter,
after the initial PTY instruction workaround. The issue was reopened. The
terminal-attached launcher workaround below is installed for new sessions;
the protected existing sessions have not been restarted. The specific
Enter-triggered burst has not been captured or accepted as fixed.

## Finding

The reproduced flashing PowerShell windows came from the managed **Codex
app-server's plain-pipe command runner**, not from a Synapse agent-spawn loop.
This host runs Codex app-server 0.157.1. Background shell processes and console
hosts are not, by themselves, additional model agents.

The operator's two Codex CLI terminals were PIDs 29880 and 139048. Both used the
shared managed app-server PID 136520; PID 74068 was its `daemon pid-update-loop`
helper. Those four PIDs and their creation times survived this work unchanged.
The `C:\code\kernelweights` session was never stopped, restarted, or steered.

Before containment, Synapse had one daemon, PID 93520, on `127.0.0.1:7700`.
Its startup task was temporarily disabled and only that verified daemon was
stopped. The operator confirmed that the windows **continued with Synapse off**.
A passive process-table observation also captured a kernelweights SSH shell
directly under Codex PID 136520 while no Synapse daemon existed.

Native window readback on this host established the distinction:

| Trigger | Shell PID | Console HWND | Separate/OS observation |
| --- | ---: | ---: | --- |
| Codex plain-pipe command, Synapse absent | 27072 | 10684664 | `IsWindowVisible=true`, parent 136520 |
| Same diagnostic with `tty: true`, Synapse absent | 90724 | 23398944 | `IsWindowVisible=false`, parent 136520 |
| PTY command after Synapse deployment | 121348 | 7998316 | Separate process/window read: live PID, parent 136520, `IsWindowVisible=false`; output `PTY-2266` |

These checks inspect native console-window state; they do not infer visibility
from a successful command return. Windows Terminal can delegate console UI to
`OpenConsole.exe`: a separate non-model `command/exec` diagnostic observed
`PseudoConsoleWindow` HWND 103353854 owned by OpenConsole PID 101148 while its
shell was PID 7416. This makes a broad window-hiding or process-killing workaround
inappropriate. An experimental visibility guard was removed after that readback;
its owned PID 52656 was stopped. No window watcher or guard is installed.

## Installed changes

1. Codex global instructions at `%USERPROFILE%\.codex\AGENTS.md` require
   `tty: true` for native Windows background `exec_command` calls, preserve
   other terminals, and prohibit unsolicited agent delegation. The active
   session received the updated instructions without a client restart.
   `scripts/install-codex-background-policy.ps1` installs this policy while
   preserving existing instructions and avoiding duplicate insertion.
2. Synapse's common `act_spawn_agent_impl` path now refuses model-agent creation
   unless the daemon environment contains **exactly**
   `SYNAPSE_AGENT_SPAWN_ENABLED=1`. MCP, dashboard, and respawn paths converge
   on this implementation. Refusal occurs before model prerequisite resolution,
   spawn-directory creation, and process launch. Request audit records can still
   be written; they are not evidence of a launched agent.
3. `health.subsystems.agent_spawn.status` reports `disabled` or `enabled`,
   including in compact health responses. This does not change tool schemas.
4. `scripts/start-pre-calyx-daemon.ps1` explicitly sets the gate to `0`, ignoring
   an inherited enabling environment variable. An operator must deliberately
   pass `-EnableAgentSpawn` to this launcher to opt in.
5. `scripts/install-codex-console-workaround.ps1` patches the npm Codex launcher
   on Windows to add `--no-daemon`. Unlike the instruction-only workaround,
   this also avoids the detached server for operations outside agent shell calls.
   It preserves explicit `--remote` connections, supports an operator opt-out
   through `SYNAPSE_CODEX_USE_SHARED_DAEMON=1`, and has a `-Restore` operation.
   The user configuration also has `features.daemon_auto_start=false`, written
   through `codex features disable daemon_auto_start`.

The PTY policy is a **Codex instruction workaround**. The additional launcher
change selects Codex's supported `--no-daemon` mode; neither patches the upstream
executable. Existing sessions still use their original shared server. An external
caller using that server's plain-pipe runner can still reproduce its window
behavior. The Synapse spawn gate is enforced in code; the PTY instruction relies
on callers following it. Ordinary shell commands still create necessary local helper
processes. This change does not claim to remove those processes or prevent
every possible API call from arbitrary shell code.

Reinstall the background-command policy from a PTY-backed shell:

```powershell
pwsh -NoProfile -File scripts/install-codex-background-policy.ps1
pwsh -NoProfile -File scripts/install-codex-console-workaround.ps1
codex features disable daemon_auto_start
```

An npm update can replace `codex.js`; reapply the launcher installer afterwards
until an upstream fix is verified. The user configuration survives npm updates.
The installer refuses unknown launcher shapes without changing them and keeps
the first original beside the launcher as `codex.js.synapse-console-backup`.
To remove only the launcher modification, use `-Restore`. Restoring the separate
configuration setting requires `codex features enable daemon_auto_start`.

The official [Codex app-server documentation](https://learn.chatgpt.com/docs/app-server)
documents PTY command execution and distinguishes `command/exec` (no model
thread/turn) from `turn/start`. The attribution above comes from this host's
process and window observations.

## Token and agent evidence

Before and after the manual rejected-spawn requests:

- `%LOCALAPPDATA%\synapse\agent-spawns` contained **703** directories. Its newest
  entry remained `agent-spawn-01a00ec7-469e-79a1-ae70-6de220cc089b`, modified
  **2026-08-17 03:12:35 -05:00**.
- The live Synapse registry reported **0 managed/killable agents**. It also
  recognized ambient clients outside Synapse's process ownership; these were
  not terminated or counted as Synapse-launched work.
- Real MCP `cost operation=summarize`, since `2026-09-27T00:00:00Z`, reported
  **0 transcript rows, 0 spawns, 0 tokens** in its exact timestamp-index window.
- `agent operation=stats` for that window initially found five state-change
  events across three observed client identities, **no tool-call events and
  no token events**. Client detection is not agent creation.

Historical retained transcripts do contain earlier agent work and fixtures.
Their aggregate is not a current burn rate. This investigation does not audit
provider billing or rule out spending by unrelated applications/accounts.

A separate read-only inspection of Codex's `%USERPROFILE%\.codex\state_5.sqlite`
at 00:56 UTC found exactly two threads created or updated since
`2026-09-27T00:00:00Z`: Synapse thread
`01a0e56e-9c76-7741-a700-ae06b15c922f` and kernelweights thread
`01a0e0d9-da13-7d70-8d5a-60d7cf901ae7`. Both had `agent_path=null` and
`agent_role=null`; no additional child-agent thread rows appeared in that window.
These two intended conversations do consume tokens. Their cumulative
`tokens_used` counters are not a provider invoice or evidence of hidden agents.

## Manual acceptance record

Verification followed D1 and #351. No automated behavioral tests, FSV scripts,
drivers, or GitHub Actions were added or run.

The real wired Codex Synapse MCP client served all 40 public tools before and
after restart, without schema errors. The surface hash stayed
`089c19716a4935b813d569ad7838af69c2cc5f6b67b7f5629bcc817a433f3149`.
The post-restart tool calls used session
`aecb9790-c218-4ce7-8f13-a559969c088c`, daemon **40400**, bind **127.0.0.1:7700**.
The existing loopback transport policy from #2265 was preserved.

For every row below, the source of truth was read before the real
`mcp__synapse.agent` call, then read again separately afterward: process table,
spawn directory count, and/or live session registry. The directory count remained
703, the four Codex PIDs remained `[29880, 74068, 136520, 139048]`, and managed
agent count remained zero.

| Case | Input | Actual result | Independent post-state |
| --- | --- | --- | --- |
| Normal disabled launch | `cli=codex`, nonempty synthetic prompt, Synapse working directory | `AGENT_SPAWN_DISABLED` | No new process/directory/managed agent |
| Empty | Empty prompt | `TOOL_PARAMS_INVALID` | Same processes and 703 directories |
| Boundary | Nonempty prompt, `wait_timeout_ms=1` | `AGENT_SPAWN_DISABLED` | Same processes and 703 directories |
| Structurally invalid | Numeric prompt `42` | `TOOL_PARAMS_INVALID` | Same processes and 703 directories; zero token readback |

The enabling branch was intentionally not used to run a billable agent.

Policy installation was separately triggered manually against physical files
under `%LOCALAPPDATA%\synapse\manual-verification\issue-2266`, with before/after
file reads:

| Case | Before | After |
| --- | --- | --- |
| Existing instructions | `Preserve this operator note.` | Note retained and policy appended |
| Empty/missing instructions | No `empty\AGENTS.md` | Policy file created; SHA256 `B3E07AB75D779F977972665871E75DCBE66651BA590204AD193D34C8AE17694E` |
| Repeated installation | Existing policy; SHA256 `FE3EE26481F285EE8B5C2D71A6B17DFF47E9659736D39F0A8E2C4487D3C339EA` | `CODEX_BACKGROUND_POLICY_PRESENT`; same hash |
| Invalid directory shape | `not-a-directory` is a file containing `Preserve this sentinel.` | Installation refused; sentinel unchanged, SHA256 `18336DE57C048A45CDFB6C46A0495742EF80B49C692AEEF9D1A4792E345C5749` |

Structural checks passed: `cargo fmt --all --check`, `cargo check -p synapse-mcp`,
and `cargo clippy --workspace --all-targets -- -D warnings`. No test runner was
invoked. The restored pre-Calyx tree retains historical test sources; this change
does not add any, and no Calyx workspace exists in the restored tree.

## Deployment

After fast compilation/lint feedback, `cargo build --release -p synapse-mcp
--bin synapse-mcp` produced the executable installed at
`%USERPROFILE%\.cargo\bin\synapse-mcp.exe`. Build and installed SHA256 matched:

`6676D88483FCA3A8A4A29585D42BCDE1DB2F9A586E59CAA1F10BD11A78E2F0F5`.

The prior binary is recoverable at
`%LOCALAPPDATA%\synapse\bin\synapse-mcp-before-2266.exe`.
`SynapseMcpPreCalyx` was re-enabled and started. Independent OS socket/process
readback and the real MCP health call agree on PID 40400. Health reports storage
and browser bridge healthy and agent spawning disabled. Both original Codex CLI
processes and the shared app-server remained alive with their original creation
times. Temporary diagnostic commands exited; no diagnostic window guard remains.

## Follow-up: automatic background server and Enter reports

The official [Codex changelog](https://learn.chatgpt.com/docs/changelog) dates
0.157.0 to September 25 and explicitly lists automatic background-server startup
for eligible interactive sessions. Installed 0.157.1 is dated September 26.
The installed CLI reports `daemon_auto_start` as stable and initially enabled.
Its `--no-daemon` help says it avoids the shared server even when already running.
This establishes a relevant recent default change, not proof that every reported
flash is caused by that release.

Read-only inspection of `logs_2.sqlite` correlated the operator's submissions at
01:29:25 and 01:29:39 UTC on September 28 with the existing Synapse conversation.
There was no corresponding new child-agent thread in `state_5.sqlite`.
PowerShellCore recorded a shell startup at the first timestamp; that historical
event does not contain its command or parent. The short-lived Enter burst was
not reproduced during the subsequent four-minute process-table capture.
Polling can miss short-lived processes. Event-based process tracing was attempted
but Windows denied access; no privileges, audit settings, or other sessions were
changed to bypass that denial.

The passive capture also found the unrelated Poly paper keeper's hidden
PowerShell task every five minutes. Its process/command records do not establish
that it caused a visible window. It was left running. The managed Codex daemon
and updater logs were empty, and the older Codex update script's last log was
from August; neither supplied evidence of a current updater restart loop.

Two additional manual, non-model `command/exec` comparisons used the same Codex
0.157.1 runtime with an inherited terminal. Neither created a thread or submitted
a model prompt. The source of truth was each child's console handle, followed by
a separate process-table and `IsWindowVisible` read while that child was alive.

| Invocation | Separate physical readback | Completion |
| --- | --- | --- |
| Direct terminal-attached stdio server PID 74900 | PowerShell PID 83492, parent 74900, HWND 29886676, invisible | Command exit 0; owned server exit 0 |
| Installed npm launcher through node PID 29252 | Codex PID 62288 command includes `--no-daemon app-server --listen stdio://`; PowerShell PID 30788, parent 62288, HWND 9111996, invisible | Command exit 0; owned launcher/server exit 0 |

These observations verify the launcher argument and attached command behavior.
They do not stand in for a user pressing Enter in a fresh interactive session.
New sessions inherit their terminal rather than using the shared background
server; background work consequently depends on that session's lifetime.

The launcher installer was manually exercised against physical files, with
separate before/after reads:

| Case | Before | Actual after |
| --- | --- | --- |
| Installed npm launcher | Exactly one upstream `spawn(binaryPath, process.argv.slice(2), ...)` site | Windows argument guard inserted; original backup retained; `node --check`, `codex --version`, and `codex mcp get synapse` succeed |
| Empty file | Zero bytes, SHA256 `E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855` | Refused; same hash |
| Unsupported structure | Sentinel comment, SHA256 `6861D8DDC48A28079FF378586C5132BEA76C3D472D4B30CDBE74F4741B7692BE` | Refused; same bytes/hash |
| Missing path | File absent | Read failed; file remained absent |
| Repeated installation | Installed launcher | Present message; unchanged SHA256 |
| Explicit restore on a copied launcher | Copy of installed launcher | Original upstream content/hash restored; live launcher untouched |

Original launcher SHA256:
`61B0194F3BB6534439C8D26A3ED57D0805F84B884588B761795323EEB92FCF70`.
Installed launcher SHA256:
`1E1B7C985028B15FAE9A1A8A710C1E81C8D432B44F677C76091A21241E99BBC6`.
PowerShell parsing and JavaScript syntax checking are structural checks, not FSV.

Final independent reads still show the two original root Codex conversations,
703 Synapse spawn directories, original CLI PIDs 29880/139048 and shared server
PID 136520 with unchanged creation times, and Synapse PID 40400 listening at
127.0.0.1:7700. The real wired Synapse MCP remains usable. No additional model
agents were launched for these diagnostics.

**Activation boundary:** the running shared server cannot be replaced without
affecting the kernelweights session. It remains intact as requested. New launches
use the installed workaround; the already-running sessions retain the PTY
instruction mitigation until their normal restart. Do not label the overall
Enter report resolved until it has been observed again after that transition.
