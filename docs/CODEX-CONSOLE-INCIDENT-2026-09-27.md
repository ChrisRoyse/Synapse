# Codex console flashes and Synapse agent-spawn policy

Issue: [#2266](https://github.com/ChrisRoyse/Synapse/issues/2266).

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

The installed policy is a **Codex instruction workaround**, not a patch to the
upstream Codex executable. An external caller that deliberately uses the
plain-pipe runner can still reproduce the upstream window behavior. The
Synapse spawn gate is enforced in code; the PTY instruction relies on callers
following it. Ordinary shell commands still create necessary local helper
processes. This change does not claim to remove those processes or prevent
every possible API call from arbitrary shell code.

Reinstall the background-command policy from a PTY-backed shell:

```powershell
pwsh -NoProfile -File scripts/install-codex-background-policy.ps1
```

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
