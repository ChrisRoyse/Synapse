# Build Speed And Disk Hygiene

This repo is tuned for fast iterative builds on the configured Windows dev host
and to prevent git worktree plus Cargo `target/` buildup from consuming the
system disk again.

## Build Settings

| Change | Where | Effect |
| --- | --- | --- |
| `rust-lld` linker | `.cargo/config.toml` | Replaces slow MSVC `link.exe` for faster Windows linking. Missing linker state fails loudly. |
| Dev incremental compilation | `Cargo.toml [profile.dev]` | Rebuilds only changed codegen units during the edit loop. |
| Dependency debuginfo off | `Cargo.toml [profile.dev.package."*"]` | Avoids large dependency debug artifacts while keeping workspace panic line tables. |
| `jobs = 32` | user Cargo config | Uses the configured host's logical cores for local builds. |

Use the fast local edit loop:

```powershell
cargo check
cargo build
```

Use `cargo build --release` only when shipping or running the optimized daemon,
not as compile feedback during edits.

## Defender Exclusions

Defender real-time scanning of Cargo output can slow local builds. The helper
self-elevates and adds the repo build-output exclusions:

```powershell
pwsh -File .\scripts\add-defender-exclusions.ps1
```

## Repo Maintenance

`scripts/repo-maintenance.ps1` is dry-run by default. It scans git repos under
`-Root` (default `C:\code`) and:

- prunes merged or remote-gone worktrees without touching active, dirty, or
  unmerged work;
- deletes local branches whose remote branch is gone;
- runs `cargo sweep` for stale build artifacts.

```powershell
pwsh -File .\scripts\repo-maintenance.ps1
pwsh -File .\scripts\repo-maintenance.ps1 -Apply
```

`scripts/install-maintenance-task.ps1` registers or removes the weekly
non-elevated Scheduled Task:

```powershell
pwsh -File .\scripts\install-maintenance-task.ps1
pwsh -File .\scripts\install-maintenance-task.ps1 -Remove
Get-ScheduledTask -TaskName SynapseRepoMaintenance
```

## Root Cause

Parallel issue work created many throwaway git worktrees, each with its own
multi-GB Cargo `target/`. Git does not automatically remove worktrees, and Cargo
does not garbage-collect old `target/` artifacts. Scheduled worktree pruning plus
`cargo sweep` keeps the checkout set and build artifacts bounded.

## MCP Helper Process Hygiene

Some external stdio MCP servers are launched as helper process trees under the
client that requested them. For the configured Exa launcher, the expected live
tree is:

```text
codex.exe or claude.exe
  -> cmd.exe ... C:\Users\hotra\.codex\bin\exa-mcp-server.cmd
      -> node.exe ... exa-mcp-server\smithery\stdio\index.cjs
```

Classify these helpers from the process table before cleanup:

- Live transport: wrapper `cmd.exe` parent is a live `codex.exe` or
  `claude.exe`, and the Exa `node.exe` parent is that wrapper PID. Leave it
  running; it belongs to an active MCP client.
- Owned probe: wrapper PID was spawned and recorded by the current operation.
  Close the MCP client first, then verify the wrapper and child PIDs are gone.
- Orphaned helper: wrapper/node command lines exactly match the Exa launcher,
  the parent client PID is absent, and no active client owns the tree. Cleanup
  may target only those exact helper PIDs after before/after process readback.

Never kill broad `cmd.exe`, terminal, IDE, WSL, Codex, or Claude process sets to
clean MCP helpers. If ownership cannot be proven, print the process Source of
Truth and leave the process running.
